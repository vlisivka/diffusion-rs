//! Tensor ops.
//!

use crate::core::{CpuStorage, DType, Layout, Module, Result, Shape, Tensor, D};
use rayon::prelude::*;

/// Applies the softmax function to the input tensor, rescaling the element so that elements on
/// a slice of fixed index on dimension `dim` are between 0 and 1 and sum to 1.
///
/// ```rust
/// use diffusion_rs_common::core::{Tensor, Device, test_utils::to_vec2_round};
/// let a = Tensor::new(&[[0f32, 1., 0., 1.], [-2., 2., 3., -3.]], &Device::Cpu)?;
/// let a = diffusion_rs_common::nn::ops::softmax(&a, 1)?;
/// assert_eq!(
///     to_vec2_round(&a, 4)?,
///     &[
///         [0.1345, 0.3655, 0.1345, 0.3655],
///         [0.0049, 0.2671, 0.7262, 0.0018]
///     ]);
/// # Ok::<(), diffusion_rs_common::core::Error>(())
/// ```
pub fn softmax<D: crate::core::shape::Dim>(xs: &Tensor, dim: D) -> Result<Tensor> {
    let dim = dim.to_index(xs.shape(), "softmax")?;
    let max = xs.max_keepdim(dim)?;
    let diff = xs.broadcast_sub(&max)?;
    let num = diff.exp()?;
    let den = num.sum_keepdim(dim)?;
    num.broadcast_div(&den)
}

pub fn log_softmax<D: crate::core::shape::Dim>(xs: &Tensor, d: D) -> Result<Tensor> {
    let d = d.to_index(xs.shape(), "log-softmax")?;
    let max = xs.max_keepdim(d)?;
    let diff = xs.broadcast_sub(&max)?;
    let sum_exp = diff.exp()?.sum_keepdim(d)?;
    let log_sm = diff.broadcast_sub(&sum_exp.log()?)?;
    Ok(log_sm)
}

pub fn silu(xs: &Tensor) -> Result<Tensor> {
    xs.silu()
}

pub fn swiglu(xs: &Tensor) -> Result<Tensor> {
    let xs = xs.chunk(2, D::Minus1)?;
    &xs[0].silu()? * &xs[1]
}

struct Sigmoid;

impl crate::core::CustomOp1 for Sigmoid {
    fn name(&self) -> &'static str {
        "sigmoid"
    }

    fn cpu_fwd(&self, storage: &CpuStorage, layout: &Layout) -> Result<(CpuStorage, Shape)> {
        use crate::core::backend::BackendStorage;

        fn fwd<T: num_traits::Float>(v: T) -> T {
            (v.neg().exp() + T::one()).recip()
        }

        // FIXME: using `crate::core::map_dtype` causes compilation errors.
        let storage = match storage {
            CpuStorage::BF16(slice) => {
                CpuStorage::BF16(crate::core::cpu_backend::unary_map(slice, layout, fwd))
            }
            CpuStorage::F16(slice) => {
                CpuStorage::F16(crate::core::cpu_backend::unary_map(slice, layout, fwd))
            }
            CpuStorage::F32(slice) => {
                CpuStorage::F32(crate::core::cpu_backend::unary_map(slice, layout, fwd))
            }
            CpuStorage::F64(slice) => {
                CpuStorage::F64(crate::core::cpu_backend::unary_map(slice, layout, fwd))
            }
            _ => Err(crate::core::Error::UnsupportedDTypeForOp(
                storage.dtype(),
                self.name(),
            ))?,
        };
        Ok((storage, layout.shape().clone()))
    }

    #[cfg(feature = "cuda")]
    fn cuda_fwd(
        &self,
        storage: &crate::core::CudaStorage,
        layout: &Layout,
    ) -> Result<(crate::core::CudaStorage, Shape)> {
        use crate::core::backend::BackendStorage;
        use crate::core::cuda_backend::cudarc::driver::{
            CudaSlice, DeviceRepr, LaunchAsync, LaunchConfig, ValidAsZeroBits,
        };
        use crate::core::cuda_backend::SlicePtrOrNull;
        use crate::core::cuda_backend::{kernel_name, kernels, Map1, WrapErr};
        use crate::core::{CudaDevice, WithDType};

        struct S;
        impl Map1 for S {
            fn f<T: DeviceRepr + WithDType + ValidAsZeroBits>(
                &self,
                src: &CudaSlice<T>,
                dev: &CudaDevice,
                layout: &Layout,
            ) -> Result<CudaSlice<T>> {
                let shape = layout.shape();
                let dims = shape.dims();
                let el_count = shape.elem_count();
                let cfg = LaunchConfig::for_num_elems(el_count as u32);
                let ds = SlicePtrOrNull::params_from_layout(dev, layout)?;
                let src = &src.slice(layout.start_offset()..);
                let func = dev.get_or_load_func(&kernel_name::<T>("usigmoid"), kernels::UNARY)?;
                // SAFETY: Set later by running the kernel.
                let out = unsafe { dev.alloc::<T>(el_count) }.w()?;

                let params = (el_count, dims.len(), &ds, src, &out);
                // SAFETY: ffi.
                unsafe { func.launch(cfg, params) }.w()?;
                Ok(out)
            }
        }

        let dev = storage.device();
        let slice = S.map(&storage.slice, dev, layout)?;
        let dst = crate::core::CudaStorage {
            slice,
            device: dev.clone(),
        };
        Ok((dst, layout.shape().clone()))
    }

    #[cfg(feature = "metal")]
    fn metal_fwd(
        &self,
        storage: &crate::core::MetalStorage,
        layout: &Layout,
    ) -> Result<(crate::core::MetalStorage, Shape)> {
        use crate::core::backend::BackendStorage;
        use crate::core::MetalError;
        let device = storage.device();
        let dtype = storage.dtype();
        let shape = layout.shape();
        let el_count = shape.elem_count();
        let buffer = device.new_buffer(el_count, dtype, "sigmoid")?;
        let command_buffer = device.command_buffer()?;
        command_buffer.set_label("sigmoid");
        let src = crate::metal_kernels::BufferOffset {
            buffer: storage.buffer(),
            offset_in_bytes: layout.start_offset() * storage.dtype().size_in_bytes(),
        };

        match (el_count % 2, dtype, layout.is_contiguous()) {
            (0, DType::BF16 | DType::F16, true) => {
                use crate::metal_kernels::unary::contiguous_tiled;
                let kernel_name = match dtype {
                    DType::F16 => contiguous_tiled::sigmoid::HALF,
                    DType::F32 => contiguous_tiled::sigmoid::FLOAT,
                    DType::BF16 => contiguous_tiled::sigmoid::BFLOAT,
                    dtype => {
                        crate::bail!(
                            "Metal contiguous_tiled unary sigmoid {dtype:?} not implemented"
                        )
                    }
                };
                crate::metal_kernels::call_unary_contiguous_tiled(
                    device.metal_device(),
                    &command_buffer,
                    device.kernels(),
                    kernel_name,
                    el_count,
                    src,
                    &buffer,
                )
                .map_err(MetalError::from)?;
            }
            (_, _, true) => {
                use crate::metal_kernels::unary::contiguous;
                let kernel_name = match dtype {
                    DType::F16 => contiguous::sigmoid::HALF,
                    DType::F32 => contiguous::sigmoid::FLOAT,
                    DType::BF16 => contiguous::sigmoid::BFLOAT,
                    dtype => {
                        crate::bail!("Metal contiguous unary sigmoid {dtype:?} not implemented")
                    }
                };
                crate::metal_kernels::call_unary_contiguous(
                    device.metal_device(),
                    &command_buffer,
                    device.kernels(),
                    kernel_name,
                    el_count,
                    src,
                    &buffer,
                )
                .map_err(MetalError::from)?;
            }
            (_, _, false) => {
                use crate::metal_kernels::unary::strided;
                let kernel_name = match dtype {
                    DType::F16 => strided::sigmoid::HALF,
                    DType::F32 => strided::sigmoid::FLOAT,
                    DType::BF16 => strided::sigmoid::BFLOAT,
                    dtype => {
                        crate::bail!("Metal strided unary sigmoid {dtype:?} not implemented")
                    }
                };
                let dst = crate::metal_kernels::BufferOffset::zero_offset(&buffer);
                crate::metal_kernels::call_unary_strided(
                    device.metal_device(),
                    &command_buffer,
                    device.kernels(),
                    kernel_name,
                    layout.dims(),
                    src,
                    layout.stride(),
                    dst,
                )
                .map_err(MetalError::from)?;
            }
        }

        let new_storage = crate::core::MetalStorage::new(buffer, device.clone(), el_count, dtype);
        Ok((new_storage, layout.shape().clone()))
    }

    fn bwd(&self, _arg: &Tensor, res: &Tensor, grad_res: &Tensor) -> Result<Option<Tensor>> {
        // d/dx sigmoid(x) = (1 - sigmoid(x)) * sigmoid(x)
        let d_dx_sigmoid = res.ones_like()?.sub(res)?.mul(res)?;
        Ok(Some(grad_res.mul(&d_dx_sigmoid)?))
    }
}

pub fn sigmoid(xs: &Tensor) -> Result<Tensor> {
    xs.apply_op1(Sigmoid)
}

pub fn hard_sigmoid(xs: &Tensor) -> Result<Tensor> {
    // TODO: Should we have a specialized op for this?
    ((xs + 3.0)? / 6.0)?.clamp(0f32, 1f32)
}

pub fn leaky_relu(xs: &Tensor, negative_slope: f64) -> Result<Tensor> {
    let zeros = xs.zeros_like()?;
    xs.maximum(&zeros)? + xs.minimum(&zeros)? * negative_slope
}

pub fn dropout(xs: &Tensor, drop_p: f32) -> Result<Tensor> {
    // This implementation is inefficient as it stores the full mask for the backward pass.
    // Instead we could just store the seed and have a specialized kernel that would both
    // generate the random mask and apply it.
    // Another easier optimization would be to be able to generate boolean mask using just a bit of
    // entropy per element rather than generating a full float per element.
    if !(0. ..1.).contains(&drop_p) {
        crate::bail!("dropout probability has to be in [0, 1), got {drop_p}")
    }
    let rand = Tensor::rand(0f32, 1f32, xs.shape(), xs.device())?;
    let scale = 1.0 / (1.0 - drop_p as f64);
    let drop_p = Tensor::new(drop_p, xs.device())?.broadcast_as(xs.shape())?;
    let mask = (rand.ge(&drop_p)?.to_dtype(xs.dtype())? * scale)?;
    xs * mask
}

#[derive(Clone, Debug)]
pub struct Dropout {
    drop_p: f32,
}

impl Dropout {
    pub fn new(drop_p: f32) -> Dropout {
        Self { drop_p }
    }

    pub fn forward(&self, xs: &Tensor, train: bool) -> Result<Tensor> {
        if train {
            dropout(xs, self.drop_p)
        } else {
            Ok(xs.clone())
        }
    }
}

impl crate::core::ModuleT for Dropout {
    fn forward_t(&self, xs: &Tensor, train: bool) -> Result<Tensor> {
        self.forward(xs, train)
    }
}

struct SoftmaxLastDim;

impl crate::core::InplaceOp1 for SoftmaxLastDim {
    fn name(&self) -> &'static str {
        "softmax-last-dim"
    }

    fn cpu_fwd(&self, storage: &mut CpuStorage, layout: &Layout) -> Result<()> {
        fn softmax<T: crate::core::WithDType + num_traits::Float>(
            src: &mut [T],
            layout: &Layout,
        ) -> Result<()> {
            let src = match layout.contiguous_offsets() {
                None => crate::bail!("input has to be contiguous"),
                Some((o1, o2)) => &mut src[o1..o2],
            };
            let dims = layout.shape().dims();
            let dim_m1 = dims[dims.len() - 1];
            src.par_chunks_mut(dim_m1).for_each(|src| {
                let mut max = T::neg_infinity();
                unsafe { T::vec_reduce_max(src.as_ptr(), &mut max, dim_m1) };
                for s in src.iter_mut() {
                    *s = (*s - max).exp();
                }
                let mut sum_exp = T::zero();
                unsafe { T::vec_reduce_sum(src.as_ptr(), &mut sum_exp, dim_m1) };
                for d in src.iter_mut() {
                    *d /= sum_exp
                }
            });
            Ok(())
        }

        match storage {
            CpuStorage::BF16(slice) => softmax::<half::bf16>(slice, layout),
            CpuStorage::F16(slice) => softmax::<half::f16>(slice, layout),
            CpuStorage::F32(slice) => softmax::<f32>(slice, layout),
            CpuStorage::F64(slice) => softmax::<f64>(slice, layout),
            _ => crate::bail!("unsupported dtype for softmax {:?}", storage),
        }
    }

    #[cfg(feature = "cuda")]
    fn cuda_fwd(&self, storage: &mut crate::core::CudaStorage, layout: &Layout) -> Result<()> {
        use crate::core::cuda_backend::cudarc::driver::{
            CudaSlice, DeviceRepr, LaunchAsync, LaunchConfig,
        };
        use crate::core::cuda_backend::{kernel_name, kernels, Map1InPlace, WrapErr};
        use crate::core::{CudaDevice, WithDType};

        struct S;
        impl Map1InPlace for S {
            fn f<T: DeviceRepr + WithDType>(
                &self,
                src: &mut CudaSlice<T>,
                dev: &CudaDevice,
                layout: &Layout,
            ) -> Result<()> {
                let src = match layout.contiguous_offsets() {
                    None => crate::bail!("input has to be contiguous"),
                    Some((o1, o2)) => src.slice(o1..o2),
                };
                let el = layout.shape().elem_count();
                let dims = layout.shape().dims();
                let dim_m1 = dims[dims.len() - 1];
                let (n_rows, n_cols) = (el / dim_m1, dim_m1);

                let func = dev.get_or_load_func(&kernel_name::<T>("softmax"), kernels::REDUCE)?;
                let cfg = LaunchConfig {
                    grid_dim: (n_rows as u32, 1, 1),
                    block_dim: (1, 32, 1),
                    shared_mem_bytes: 0,
                };
                let params = (&src, &src, n_cols as i32);
                // SAFETY: ffi.
                unsafe { func.launch(cfg, params) }.w()?;
                Ok(())
            }
        }

        use crate::core::backend::BackendStorage;
        let dev = storage.device().clone();

        S.map(&mut storage.slice, &dev, layout)?;

        Ok(())
    }

    #[cfg(feature = "metal")]
    fn metal_fwd(&self, storage: &mut crate::core::MetalStorage, layout: &Layout) -> Result<()> {
        use crate::core::backend::BackendStorage;
        let device = storage.device();
        let command_buffer = device.command_buffer()?;
        let kernels = device.kernels();
        let name = match storage.dtype() {
            DType::F32 => "softmax_f32",
            DType::F16 => "softmax_f16",
            DType::BF16 => "softmax_bf16",
            dtype => crate::bail!("softmax-last-dim is not implemented for {dtype:?}"),
        };

        let n = layout.stride().len();
        if !(layout.is_contiguous() && layout.stride()[n - 1] == 1) {
            crate::bail!("Non contiguous softmax-last-dim is not implemented");
        }

        let last_dim = layout.dims()[layout.shape().rank() - 1];
        let elem_count = layout.shape().elem_count();
        crate::metal_kernels::call_last_softmax(
            device.metal_device(),
            &command_buffer,
            kernels,
            name,
            elem_count,
            last_dim,
            storage.buffer(),
            layout.start_offset() * storage.dtype().size_in_bytes(),
            storage.buffer(),
            layout.start_offset() * storage.dtype().size_in_bytes(),
        )
        .map_err(crate::core::Error::wrap)?;
        Ok(())
    }
}

impl crate::core::CustomOp1 for SoftmaxLastDim {
    fn name(&self) -> &'static str {
        "softmax-last-dim"
    }

    fn cpu_fwd(&self, storage: &CpuStorage, layout: &Layout) -> Result<(CpuStorage, Shape)> {
        fn softmax<T: crate::core::WithDType + num_traits::Float>(
            src: &[T],
            layout: &Layout,
        ) -> Result<(CpuStorage, Shape)> {
            let src = match layout.contiguous_offsets() {
                None => crate::bail!("input has to be contiguous"),
                Some((o1, o2)) => &src[o1..o2],
            };
            let el_count = layout.shape().elem_count();
            let dims = layout.shape().dims();
            let dim_m1 = dims[dims.len() - 1];
            let mut dst = vec![T::zero(); el_count];
            src.par_chunks(dim_m1)
                .zip(dst.par_chunks_mut(dim_m1))
                .for_each(|(src, dst)| {
                    let mut max = T::neg_infinity();
                    unsafe { T::vec_reduce_max(src.as_ptr(), &mut max, dim_m1) };
                    for (s, d) in src.iter().zip(dst.iter_mut()) {
                        *d = (*s - max).exp();
                    }
                    let mut sum_exp = T::zero();
                    unsafe { T::vec_reduce_sum(dst.as_ptr(), &mut sum_exp, dim_m1) };
                    for d in dst.iter_mut() {
                        *d /= sum_exp
                    }
                });
            let storage = crate::core::WithDType::to_cpu_storage_owned(dst);
            Ok((storage, Shape::from_dims(dims)))
        }

        match storage {
            CpuStorage::BF16(slice) => softmax::<half::bf16>(slice, layout),
            CpuStorage::F16(slice) => softmax::<half::f16>(slice, layout),
            CpuStorage::F32(slice) => softmax::<f32>(slice, layout),
            CpuStorage::F64(slice) => softmax::<f64>(slice, layout),
            _ => crate::bail!("unsupported dtype for softmax {:?}", storage),
        }
    }

    #[cfg(feature = "cuda")]
    fn cuda_fwd(
        &self,
        storage: &crate::core::CudaStorage,
        layout: &Layout,
    ) -> Result<(crate::core::CudaStorage, Shape)> {
        use crate::core::cuda_backend::cudarc::driver::{
            CudaSlice, DeviceRepr, LaunchAsync, LaunchConfig,
        };
        use crate::core::cuda_backend::{kernel_name, kernels, Map1, WrapErr};
        use crate::core::{CudaDevice, WithDType};

        struct S;
        impl Map1 for S {
            fn f<T: DeviceRepr + WithDType>(
                &self,
                src: &CudaSlice<T>,
                dev: &CudaDevice,
                layout: &Layout,
            ) -> Result<CudaSlice<T>> {
                let src = match layout.contiguous_offsets() {
                    None => crate::bail!("input has to be contiguous"),
                    Some((o1, o2)) => src.slice(o1..o2),
                };
                let el = layout.shape().elem_count();
                let dims = layout.shape().dims();
                let dim_m1 = dims[dims.len() - 1];
                let (n_rows, n_cols) = (el / dim_m1, dim_m1);

                let cfg = LaunchConfig {
                    grid_dim: (n_rows as u32, 1, 1),
                    block_dim: (1, 32, 1),
                    shared_mem_bytes: 0,
                };
                let func = dev.get_or_load_func(&kernel_name::<T>("softmax"), kernels::REDUCE)?;
                // SAFETY: Set later by running the kernel.
                let dst = unsafe { dev.alloc::<T>(el) }.w()?;
                let params = (&src, &dst, n_cols as i32);
                // SAFETY: ffi.
                unsafe { func.launch(cfg, params) }.w()?;
                Ok(dst)
            }
        }

        use crate::core::backend::BackendStorage;
        let dev = storage.device();
        let slice = S.map(&storage.slice, dev, layout)?;
        let dst = crate::core::cuda_backend::CudaStorage {
            slice,
            device: dev.clone(),
        };
        Ok((dst, layout.shape().clone()))
    }

    #[cfg(feature = "metal")]
    fn metal_fwd(
        &self,
        storage: &crate::core::MetalStorage,
        layout: &Layout,
    ) -> Result<(crate::core::MetalStorage, Shape)> {
        use crate::core::backend::BackendStorage;
        let device = storage.device();
        let command_buffer = device.command_buffer()?;
        let kernels = device.kernels();
        let name = match storage.dtype() {
            DType::F32 => "softmax_f32",
            DType::F16 => "softmax_f16",
            DType::BF16 => "softmax_bf16",
            dtype => crate::bail!("softmax-last-dim is not implemented for {dtype:?}"),
        };

        let n = layout.stride().len();
        if !(layout.is_contiguous() && layout.stride()[n - 1] == 1) {
            crate::bail!("Non contiguous softmax-last-dim is not implemented");
        }

        let last_dim = layout.dims()[layout.shape().rank() - 1];
        let elem_count = layout.shape().elem_count();
        let output = device.new_buffer(elem_count, storage.dtype(), "softmax")?;
        crate::metal_kernels::call_last_softmax(
            device.metal_device(),
            &command_buffer,
            kernels,
            name,
            elem_count,
            last_dim,
            storage.buffer(),
            layout.start_offset() * storage.dtype().size_in_bytes(),
            &output,
            0,
        )
        .map_err(crate::core::Error::wrap)?;
        let newstorage =
            crate::core::MetalStorage::new(output, device.clone(), elem_count, storage.dtype());
        Ok((newstorage, layout.shape().clone()))
    }
}

pub fn softmax_last_dim(xs: &Tensor) -> Result<Tensor> {
    xs.apply_op1_no_bwd(&SoftmaxLastDim)
}

pub fn inplace_softmax_last_dim(xs: &mut Tensor) -> Result<()> {
    xs.inplace_op1(&SoftmaxLastDim)
}

// TODO: need cpu and cuda impls
#[allow(dead_code)]
struct AttnSoftmaxLastDim {
    scale: f32,
}

impl crate::core::InplaceOp2 for AttnSoftmaxLastDim {
    fn name(&self) -> &'static str {
        "attn-softmax-last-dim"
    }

    fn cpu_fwd(
        &self,
        _a_s: &mut CpuStorage,
        _a_l: &Layout,
        _mask_s: &CpuStorage,
        _mask_l: &Layout,
    ) -> Result<()> {
        crate::bail!("cpu attn-softmax-last-dim is not implemented");
    }

    #[cfg(feature = "metal")]
    fn metal_fwd(
        &self,
        a_s: &mut crate::core::MetalStorage,
        a_l: &Layout,
        mask_s: &crate::core::MetalStorage,
        mask_l: &Layout,
    ) -> Result<()> {
        use crate::core::backend::BackendStorage;
        let device = a_s.device();
        let command_buffer = device.command_buffer()?;
        let kernels = device.kernels();

        let ty = match a_s.dtype() {
            DType::F32 => crate::metal_kernels::SdpaDType::F32,
            DType::F16 => crate::metal_kernels::SdpaDType::F16,
            DType::BF16 => crate::metal_kernels::SdpaDType::BF16,
            dtype => crate::bail!("attn-softmax-last-dim is not implemented for {dtype:?}"),
        };

        if !a_l.is_contiguous() {
            crate::bail!("Non contiguous xs for attn-softmax-last-dim is not implemented");
        }
        if !mask_l.is_contiguous() {
            crate::bail!("Non contiguous mask for attn-softmax-last-dim is not implemented");
        }

        if a_l.dims().len() != 4 {
            crate::bail!("attn-softmax-last-dim expects xs of rank 2");
        }
        if mask_l.dims().len() != 2 {
            crate::bail!("attn-softmax-last-dim expects mask of rank 2");
        }
        if mask_l.dim(D::Minus1)? != a_l.dim(D::Minus1)?
            || mask_l.dim(D::Minus2)? != a_l.dim(D::Minus2)?
        {
            crate::bail!("attn-softmax-last-dim expects last 2 dims to match xs last 2 dims");
        }

        crate::metal_kernels::call_last_attn_softmax(
            device.metal_device(),
            &command_buffer,
            kernels,
            a_s.buffer(),
            a_l.start_offset() * a_s.dtype().size_in_bytes(),
            mask_s.buffer(),
            mask_l.start_offset() * mask_s.dtype().size_in_bytes(),
            a_l.dims(),
            self.scale,
            ty,
            a_s.buffer(),
            0,
        )
        .map_err(crate::core::Error::wrap)?;

        Ok(())
    }
}

impl crate::core::CustomOp2 for AttnSoftmaxLastDim {
    fn name(&self) -> &'static str {
        "attn-softmax-last-dim"
    }

    fn cpu_fwd(
        &self,
        _a_s: &CpuStorage,
        _a_l: &Layout,
        _mask_s: &CpuStorage,
        _mask_l: &Layout,
    ) -> Result<(CpuStorage, Shape)> {
        crate::bail!("cpu attn-softmax-last-dim is not implemented");
    }

    #[cfg(feature = "metal")]
    fn metal_fwd(
        &self,
        a_s: &crate::core::MetalStorage,
        a_l: &Layout,
        mask_s: &crate::core::MetalStorage,
        mask_l: &Layout,
    ) -> Result<(crate::core::MetalStorage, Shape)> {
        use crate::core::backend::BackendStorage;
        let device = a_s.device();
        let command_buffer = device.command_buffer()?;
        let kernels = device.kernels();

        let ty = match a_s.dtype() {
            DType::F32 => crate::metal_kernels::SdpaDType::F32,
            DType::F16 => crate::metal_kernels::SdpaDType::F16,
            DType::BF16 => crate::metal_kernels::SdpaDType::BF16,
            dtype => crate::bail!("attn-softmax-last-dim is not implemented for {dtype:?}"),
        };

        if !a_l.is_contiguous() {
            crate::bail!("Non contiguous xs for attn-softmax-last-dim is not implemented");
        }
        if !mask_l.is_contiguous() {
            crate::bail!("Non contiguous mask for attn-softmax-last-dim is not implemented");
        }

        if a_l.dims().len() != 4 {
            crate::bail!("attn-softmax-last-dim expects xs of rank 2");
        }
        if mask_l.dims().len() != 2 {
            crate::bail!("attn-softmax-last-dim expects mask of rank 2");
        }
        if mask_l.dim(D::Minus1)? != a_l.dim(D::Minus1)?
            || mask_l.dim(D::Minus2)? != a_l.dim(D::Minus2)?
        {
            crate::bail!("attn-softmax-last-dim expects last 2 dims to match xs last 2 dims");
        }

        let elem_count = a_l.shape().elem_count();
        let output = device.new_buffer(elem_count, a_s.dtype(), "attn-softmax")?;
        crate::metal_kernels::call_last_attn_softmax(
            device.metal_device(),
            &command_buffer,
            kernels,
            a_s.buffer(),
            a_l.start_offset() * a_s.dtype().size_in_bytes(),
            mask_s.buffer(),
            mask_l.start_offset() * mask_s.dtype().size_in_bytes(),
            a_l.dims(),
            self.scale,
            ty,
            &output,
            a_l.start_offset() * a_s.dtype().size_in_bytes(),
        )
        .map_err(crate::core::Error::wrap)?;
        let newstorage =
            crate::core::MetalStorage::new(output, device.clone(), elem_count, a_s.dtype());
        Ok((newstorage, a_l.shape().clone()))
    }
}

/// Softmax with fused broadcast addition of a mask and scale.
/// Equivalent to:
/// ```ignore
/// diffusion_rs_common::nn::ops::softmax_last_dim(&(xs.broadcast_add(&mask)? * scale as f64)?)?
/// ```
/// - `xs` must be a rank-4 tensor
/// - `mask` must be a rank-2 matrix
/// - The last 2 dimensions of `xs` must match the dimensions of `mask`.
///
/// Note: if the last dim of `xs` is a multiple of 4, a vectorized implementation will be used.
pub fn attn_softmax_last_dim(xs: &Tensor, mask: &Tensor, scale: f32) -> Result<Tensor> {
    if xs.device().is_metal() {
        xs.apply_op2_no_bwd(mask, &AttnSoftmaxLastDim { scale })
    } else {
        softmax_last_dim(&(xs.broadcast_add(mask)? * scale as f64)?)
    }
}

/// Inplace equivalent of `attn_softmax_last_dim`
pub fn inplace_attn_softmax_last_dim(xs: &mut Tensor, mask: &Tensor, scale: f32) -> Result<()> {
    if xs.device().is_metal() {
        xs.inplace_op2(mask, &AttnSoftmaxLastDim { scale })?;
    } else {
        *xs = softmax_last_dim(&(xs.broadcast_add(mask)? * scale as f64)?)?;
    }
    Ok(())
}

#[derive(Debug, Clone)]
struct RmsNorm {
    eps: f32,
}

impl crate::core::CustomOp2 for RmsNorm {
    fn name(&self) -> &'static str {
        "rms-norm"
    }

    fn cpu_fwd(
        &self,
        s1: &CpuStorage,
        l1: &Layout,
        s2: &CpuStorage,
        l2: &Layout,
    ) -> Result<(CpuStorage, Shape)> {
        use crate::core::backend::BackendStorage;

        let eps = self.eps;
        fn inner<
            T: crate::core::WithDType
                + num_traits::Float
                + num_traits::AsPrimitive<f32>
                + num_traits::FromPrimitive,
        >(
            src: &[T],
            layout: &Layout,
            alpha: &[T],
            alpha_layout: &Layout,
            eps: f32,
        ) -> Result<(CpuStorage, Shape)> {
            let src = match layout.contiguous_offsets() {
                None => crate::bail!("input has to be contiguous"),
                Some((o1, o2)) => &src[o1..o2],
            };
            let alpha = match alpha_layout.contiguous_offsets() {
                None => crate::bail!("alpha has to be contiguous"),
                Some((o1, o2)) => &alpha[o1..o2],
            };
            let el_count = layout.shape().elem_count();
            let dims = layout.shape().dims();
            let dim_m1 = dims[dims.len() - 1];
            let mut dst = vec![T::zero(); el_count];
            src.par_chunks(dim_m1)
                .zip(dst.par_chunks_mut(dim_m1))
                .for_each(|(src, dst)| {
                    let sum2 = src
                        .iter()
                        .map(|&v| {
                            let v = v.as_();
                            v * v
                        })
                        .sum::<f32>();
                    let m = (sum2 / dim_m1 as f32 + eps).sqrt();
                    let m = T::from_f32(m).unwrap_or_else(T::nan);
                    for ((d, s), alpha) in dst.iter_mut().zip(src.iter()).zip(alpha) {
                        *d = *s / m * *alpha
                    }
                });
            let storage = crate::core::WithDType::to_cpu_storage_owned(dst);
            Ok((storage, Shape::from_dims(dims)))
        }

        use CpuStorage as C;
        match (s1, s2) {
            (C::BF16(s1), C::BF16(s2)) => inner::<half::bf16>(s1, l1, s2, l2, eps),
            (C::F16(s1), C::F16(s2)) => inner::<half::f16>(s1, l1, s2, l2, eps),
            (C::F32(s1), C::F32(s2)) => inner::<f32>(s1, l1, s2, l2, eps),
            _ => crate::bail!("unsupported dtype for rmsnorm {:?}", s1.dtype()),
        }
    }

    #[cfg(feature = "cuda")]
    fn cuda_fwd(
        &self,
        s1: &crate::core::CudaStorage,
        l1: &Layout,
        s2: &crate::core::CudaStorage,
        l2: &Layout,
    ) -> Result<(crate::core::CudaStorage, Shape)> {
        use crate::core::cuda_backend::cudarc::driver::{
            CudaSlice, DeviceRepr, LaunchAsync, LaunchConfig,
        };
        use crate::core::cuda_backend::{kernel_name, kernels, Map2, WrapErr};
        use crate::core::{CudaDevice, WithDType};

        struct S {
            eps: f32,
        }
        impl Map2 for S {
            fn f<T: DeviceRepr + WithDType>(
                &self,
                src: &CudaSlice<T>,
                layout: &Layout,
                alpha: &CudaSlice<T>,
                alpha_layout: &Layout,
                dev: &CudaDevice,
            ) -> Result<CudaSlice<T>> {
                let src = match layout.contiguous_offsets() {
                    None => crate::bail!("input has to be contiguous"),
                    Some((o1, o2)) => src.slice(o1..o2),
                };
                let alpha = match alpha_layout.contiguous_offsets() {
                    None => crate::bail!("alpha has to be contiguous"),
                    Some((o1, o2)) => alpha.slice(o1..o2),
                };
                let el = layout.shape().elem_count();
                let dims = layout.shape().dims();
                let dim_m1 = dims[dims.len() - 1];
                let (n_rows, n_cols) = (el / dim_m1, dim_m1);

                let block_size = if n_cols < 1024 { 32 } else { 1024 };
                let cfg = LaunchConfig {
                    grid_dim: (n_rows as u32, 1, 1),
                    block_dim: (block_size, 1, 1),
                    shared_mem_bytes: 0,
                };
                let func = dev.get_or_load_func(&kernel_name::<T>("rmsnorm"), kernels::REDUCE)?;
                // SAFETY: Set later by running the kernel.
                let dst = unsafe { dev.alloc::<T>(el) }.w()?;
                let params = (
                    &src,
                    &dst,
                    &alpha,
                    n_cols as i32,
                    block_size as i32,
                    self.eps,
                );
                // SAFETY: ffi.
                unsafe { func.launch(cfg, params) }.w()?;
                Ok(dst)
            }
        }

        use crate::core::backend::BackendStorage;
        let dev = s1.device();
        let slice = S { eps: self.eps }.map(&s1.slice, l1, &s2.slice, l2, dev)?;
        let dst = crate::core::cuda_backend::CudaStorage {
            slice,
            device: dev.clone(),
        };
        Ok((dst, l1.shape().clone()))
    }

    #[cfg(feature = "metal")]
    fn metal_fwd(
        &self,
        s1: &crate::core::MetalStorage,
        l1: &Layout,
        s2: &crate::core::MetalStorage,
        l2: &Layout,
    ) -> Result<(crate::core::MetalStorage, Shape)> {
        use crate::core::backend::BackendStorage;
        let device = s1.device();
        let command_buffer = device.command_buffer()?;
        let kernels = device.kernels();
        let name = match (s1.dtype(), s2.dtype()) {
            (DType::F32, DType::F32) => "rmsnorm_f32",
            (DType::F16, DType::F16) => "rmsnorm_f16",
            (DType::BF16, DType::BF16) => "rmsnorm_bf16",
            (dt1, dt2) => crate::bail!("rmsnorm is not implemented for {dt1:?} {dt2:?}"),
        };

        if !(l1.is_contiguous() && l2.is_contiguous()) {
            crate::bail!("Non contiguous rmsnorm is not implemented");
        }

        let last_dim = l1.dims()[l1.shape().rank() - 1];
        let elem_count = l1.shape().elem_count();
        let output = device.new_buffer(elem_count, s1.dtype(), "rmsnorm")?;
        crate::metal_kernels::call_rms_norm(
            device.metal_device(),
            &command_buffer,
            kernels,
            name,
            elem_count,
            last_dim,
            self.eps,
            s1.buffer(),
            l1.start_offset() * s1.dtype().size_in_bytes(),
            s2.buffer(),
            l2.start_offset() * s2.dtype().size_in_bytes(),
            &output,
        )
        .map_err(crate::core::Error::wrap)?;
        let newstorage =
            crate::core::MetalStorage::new(output, device.clone(), elem_count, s1.dtype());
        Ok((newstorage, l1.shape().clone()))
    }
}

pub fn rms_norm_slow(x: &Tensor, alpha: &Tensor, eps: f32) -> Result<Tensor> {
    let x_dtype = x.dtype();
    let internal_dtype = match x_dtype {
        DType::F16 | DType::BF16 => DType::F32,
        d => d,
    };
    let hidden_size = x.dim(D::Minus1)?;
    let x = x.to_dtype(internal_dtype)?;
    let norm_x = (x.sqr()?.sum_keepdim(D::Minus1)? / hidden_size as f64)?;
    let x_normed = x.broadcast_div(&(norm_x + eps as f64)?.sqrt()?)?;
    x_normed.to_dtype(x_dtype)?.broadcast_mul(alpha)
}

pub fn rms_norm(xs: &Tensor, alpha: &Tensor, eps: f32) -> Result<Tensor> {
    let hidden_size_xs = xs.dim(D::Minus1)?;
    let hidden_size_alpha = alpha.dims1()?;
    if hidden_size_xs != hidden_size_alpha {
        crate::bail!(
            "shape mismatch in rms-norm {:?} {:?}",
            xs.shape(),
            alpha.shape()
        )
    }
    xs.apply_op2_no_bwd(alpha, &RmsNorm { eps })
}

#[derive(Debug, Clone)]
struct LayerNorm {
    eps: f32,
}

impl crate::core::CustomOp3 for LayerNorm {
    fn name(&self) -> &'static str {
        "layer-norm"
    }

    fn cpu_fwd(
        &self,
        s1: &CpuStorage,
        l1: &Layout,
        s2: &CpuStorage,
        l2: &Layout,
        s3: &CpuStorage,
        l3: &Layout,
    ) -> Result<(CpuStorage, Shape)> {
        use crate::core::backend::BackendStorage;

        let eps = self.eps;
        fn inner<
            T: crate::core::WithDType
                + num_traits::Float
                + num_traits::AsPrimitive<f32>
                + num_traits::FromPrimitive,
        >(
            src: &[T],
            layout: &Layout,
            alpha: &[T],
            alpha_layout: &Layout,
            beta: &[T],
            beta_layout: &Layout,
            eps: f32,
        ) -> Result<(CpuStorage, Shape)> {
            let src = match layout.contiguous_offsets() {
                None => crate::bail!("input has to be contiguous"),
                Some((o1, o2)) => &src[o1..o2],
            };
            let alpha = match alpha_layout.contiguous_offsets() {
                None => crate::bail!("alpha has to be contiguous"),
                Some((o1, o2)) => &alpha[o1..o2],
            };
            let beta = match beta_layout.contiguous_offsets() {
                None => crate::bail!("beta has to be contiguous"),
                Some((o1, o2)) => &beta[o1..o2],
            };
            let el_count = layout.shape().elem_count();
            let dims = layout.shape().dims();
            let dim_m1 = dims[dims.len() - 1];
            let mut dst = vec![T::zero(); el_count];
            src.par_chunks(dim_m1)
                .zip(dst.par_chunks_mut(dim_m1))
                .for_each(|(src, dst)| {
                    let mut sum = 0f32;
                    let mut sum2 = 0f32;
                    for v in src {
                        let v = v.as_();
                        sum += v;
                        sum2 += v * v;
                    }
                    let mean = sum / dim_m1 as f32;
                    let var = sum2 / dim_m1 as f32 - mean * mean;
                    let inv_std = (var + eps).sqrt().recip();
                    for ((d, s), (alpha, beta)) in
                        dst.iter_mut().zip(src.iter()).zip(alpha.iter().zip(beta))
                    {
                        let alpha = alpha.as_();
                        let beta = beta.as_();
                        let d_ = (s.as_() - mean) * inv_std * alpha + beta;
                        *d = T::from_f32(d_).unwrap_or_else(T::nan);
                    }
                });
            let storage = crate::core::WithDType::to_cpu_storage_owned(dst);
            Ok((storage, Shape::from_dims(dims)))
        }

        use CpuStorage as C;
        match (s1, s2, s3) {
            (C::BF16(s1), C::BF16(s2), C::BF16(s3)) => {
                inner::<half::bf16>(s1, l1, s2, l2, s3, l3, eps)
            }
            (C::F16(s1), C::F16(s2), C::F16(s3)) => inner::<half::f16>(s1, l1, s2, l2, s3, l3, eps),
            (C::F32(s1), C::F32(s2), C::F32(s3)) => inner::<f32>(s1, l1, s2, l2, s3, l3, eps),
            _ => crate::bail!("unsupported dtype for rmsnorm {:?}", s1.dtype()),
        }
    }

    #[cfg(feature = "cuda")]
    fn cuda_fwd(
        &self,
        s1: &crate::core::CudaStorage,
        l1: &Layout,
        s2: &crate::core::CudaStorage,
        l2: &Layout,
        s3: &crate::core::CudaStorage,
        l3: &Layout,
    ) -> Result<(crate::core::CudaStorage, Shape)> {
        use crate::core::cuda_backend::cudarc::driver::{
            CudaSlice, DeviceRepr, LaunchAsync, LaunchConfig,
        };
        use crate::core::cuda_backend::{kernel_name, kernels, Map3, WrapErr};
        use crate::core::{CudaDevice, WithDType};

        struct S {
            eps: f32,
        }
        impl Map3 for S {
            fn f<T: DeviceRepr + WithDType>(
                &self,
                src: &CudaSlice<T>,
                layout: &Layout,
                alpha: &CudaSlice<T>,
                alpha_layout: &Layout,
                beta: &CudaSlice<T>,
                beta_layout: &Layout,
                dev: &CudaDevice,
            ) -> Result<CudaSlice<T>> {
                let src = match layout.contiguous_offsets() {
                    None => crate::bail!("input has to be contiguous"),
                    Some((o1, o2)) => src.slice(o1..o2),
                };
                let alpha = match alpha_layout.contiguous_offsets() {
                    None => crate::bail!("alpha has to be contiguous"),
                    Some((o1, o2)) => alpha.slice(o1..o2),
                };
                let beta = match beta_layout.contiguous_offsets() {
                    None => crate::bail!("beta has to be contiguous"),
                    Some((o1, o2)) => beta.slice(o1..o2),
                };
                let el = layout.shape().elem_count();
                let dims = layout.shape().dims();
                let dim_m1 = dims[dims.len() - 1];
                let (n_rows, n_cols) = (el / dim_m1, dim_m1);

                let block_size = if n_cols < 1024 { 32 } else { 1024 };
                let cfg = LaunchConfig {
                    grid_dim: (n_rows as u32, 1, 1),
                    block_dim: (block_size, 1, 1),
                    shared_mem_bytes: 0,
                };
                let func = dev.get_or_load_func(&kernel_name::<T>("layernorm"), kernels::REDUCE)?;
                // SAFETY: Set later by running the kernel.
                let dst = unsafe { dev.alloc::<T>(el) }.w()?;
                let params = (
                    &src,
                    &dst,
                    &alpha,
                    &beta,
                    n_cols as i32,
                    block_size as i32,
                    self.eps,
                );
                // SAFETY: ffi.
                unsafe { func.launch(cfg, params) }.w()?;
                Ok(dst)
            }
        }

        use crate::core::backend::BackendStorage;
        let dev = s1.device();
        let slice = S { eps: self.eps }.map(&s1.slice, l1, &s2.slice, l2, &s3.slice, l3, dev)?;
        let dst = crate::core::cuda_backend::CudaStorage {
            slice,
            device: dev.clone(),
        };
        Ok((dst, l1.shape().clone()))
    }

    #[cfg(feature = "metal")]
    fn metal_fwd(
        &self,
        s1: &crate::core::MetalStorage,
        l1: &Layout,
        s2: &crate::core::MetalStorage,
        l2: &Layout,
        s3: &crate::core::MetalStorage,
        l3: &Layout,
    ) -> Result<(crate::core::MetalStorage, Shape)> {
        use crate::core::backend::BackendStorage;
        let device = s1.device();
        let command_buffer = device.command_buffer()?;
        let kernels = device.kernels();
        let name = match (s1.dtype(), s2.dtype(), s3.dtype()) {
            (DType::F32, DType::F32, DType::F32) => "layernorm_f32",
            (DType::F16, DType::F16, DType::F16) => "layernorm_f16",
            (DType::BF16, DType::BF16, DType::BF16) => "layernorm_bf16",
            (dt1, dt2, dt3) => {
                crate::bail!("layernorm is not implemented for {dt1:?} {dt2:?} {dt3:?}")
            }
        };

        if !(l1.is_contiguous() && l2.is_contiguous() && l3.is_contiguous()) {
            crate::bail!("Non contiguous layernorm is not implemented");
        }

        let last_dim = l1.dims()[l1.shape().rank() - 1];
        let elem_count = l1.shape().elem_count();
        let output = device.new_buffer(elem_count, s1.dtype(), "layernorm")?;
        crate::metal_kernels::call_layer_norm(
            device.metal_device(),
            &command_buffer,
            kernels,
            name,
            elem_count,
            last_dim,
            self.eps,
            s1.buffer(),
            l1.start_offset() * s1.dtype().size_in_bytes(),
            s2.buffer(),
            l2.start_offset() * s2.dtype().size_in_bytes(),
            s3.buffer(),
            l3.start_offset() * s3.dtype().size_in_bytes(),
            &output,
        )
        .map_err(crate::core::Error::wrap)?;
        let newstorage =
            crate::core::MetalStorage::new(output, device.clone(), elem_count, s1.dtype());
        Ok((newstorage, l1.shape().clone()))
    }
}

pub fn layer_norm_slow(x: &Tensor, alpha: &Tensor, beta: &Tensor, eps: f32) -> Result<Tensor> {
    let x_dtype = x.dtype();
    let internal_dtype = match x_dtype {
        DType::F16 | DType::BF16 => DType::F32,
        d => d,
    };
    let hidden_size = x.dim(D::Minus1)?;
    let x = x.to_dtype(internal_dtype)?;
    let x = {
        let mean_x = (x.sum_keepdim(D::Minus1)? / hidden_size as f64)?;
        x.broadcast_sub(&mean_x)?
    };
    let norm_x = (x.sqr()?.sum_keepdim(D::Minus1)? / hidden_size as f64)?;
    let x_normed = x.broadcast_div(&(norm_x + eps as f64)?.sqrt()?)?;
    x_normed
        .to_dtype(x_dtype)?
        .broadcast_mul(alpha)?
        .broadcast_add(beta)
}

pub fn layer_norm(xs: &Tensor, alpha: &Tensor, beta: &Tensor, eps: f32) -> Result<Tensor> {
    let hidden_size_xs = xs.dim(D::Minus1)?;
    let hidden_size_alpha = alpha.dims1()?;
    let hidden_size_beta = beta.dims1()?;
    if hidden_size_xs != hidden_size_alpha || hidden_size_xs != hidden_size_beta {
        crate::bail!(
            "shape mismatch in layer-norm src: {:?} alpha: {:?} beta: {:?}",
            xs.shape(),
            alpha.shape(),
            beta.shape()
        )
    }
    xs.apply_op3_no_bwd(alpha, beta, &LayerNorm { eps })
}

// https://pytorch.org/docs/stable/generated/torch.nn.PixelShuffle.html
pub fn pixel_shuffle(xs: &Tensor, upscale_factor: usize) -> Result<Tensor> {
    let (b_size, c, h, w) = xs.dims4()?;
    let out_c = c / upscale_factor / upscale_factor;
    xs.reshape((b_size, out_c, upscale_factor, upscale_factor, h, w))?
        .permute((0, 1, 4, 2, 5, 3))?
        .reshape((b_size, out_c, h * upscale_factor, w * upscale_factor))
}

pub fn pixel_unshuffle(xs: &Tensor, downscale_factor: usize) -> Result<Tensor> {
    let (b_size, c, h, w) = xs.dims4()?;
    let out_c = c * downscale_factor * downscale_factor;
    xs.reshape((
        b_size,
        c,
        h / downscale_factor,
        downscale_factor,
        w / downscale_factor,
        downscale_factor,
    ))?
    .permute((0, 1, 3, 5, 2, 4))?
    .reshape((b_size, out_c, h / downscale_factor, w / downscale_factor))
}

// https://pytorch.org/docs/stable/generated/torch.nn.ReplicationPad2d.html
pub fn replication_pad2d(xs: &Tensor, pad: usize) -> Result<Tensor> {
    match pad {
        0 => Ok(xs.clone()),
        1 => {
            let (_b_size, _c, h, w) = xs.dims4()?;
            let (first, last) = (xs.narrow(3, 0, 1)?, xs.narrow(3, w - 1, 1)?);
            let xs = Tensor::cat(&[&first, xs, &last], 3)?;
            let (first, last) = (xs.narrow(2, 0, 1)?, xs.narrow(2, h - 1, 1)?);
            Tensor::cat(&[&first, &xs, &last], 2)
        }
        n => crate::bail!("replication-pad with a size of {n} is not supported"),
    }
}

#[cfg(feature = "cuda")]
pub fn kvconcat(ltensor: &Tensor, rtensor: &Tensor, concat_dim: usize) -> Result<Tensor> {
    if !ltensor.device().is_cuda() {
        return Tensor::cat(&[ltensor, rtensor], concat_dim)?.contiguous();
    }
    use crate::core::cuda_backend::KVConcat;
    let op = KVConcat { concat_dim };
    //inputs for kvconcat must be contiguous tensors
    if ltensor.is_contiguous() && rtensor.is_contiguous() {
        ltensor.apply_op2(rtensor, op)
    } else if ltensor.is_contiguous() {
        ltensor.apply_op2(&rtensor.contiguous()?, op)
    } else if rtensor.is_contiguous() {
        let ltensor = ltensor.contiguous()?;
        ltensor.apply_op2(rtensor, op)
    } else {
        let ltensor = ltensor.contiguous()?;
        let rtensor = rtensor.contiguous()?;
        ltensor.apply_op2(&rtensor, op)
    }
}

#[cfg(not(feature = "cuda"))]
pub fn kvconcat(ltensor: &Tensor, rtensor: &Tensor, concat_dim: i32) -> Result<Tensor> {
    Tensor::cat(&[ltensor, rtensor], concat_dim as usize)?.contiguous()
}

#[derive(Clone, Debug)]
pub struct Identity;

impl Identity {
    pub fn new() -> Identity {
        Self
    }
}

impl Default for Identity {
    fn default() -> Self {
        Self
    }
}

impl Module for Identity {
    fn forward(&self, xs: &Tensor) -> Result<Tensor> {
        Ok(xs.clone())
    }
}

#[allow(dead_code)]
struct Sdpa {
    scale: f32,
    softcapping: f32,
}

impl crate::core::CustomOp3 for Sdpa {
    fn name(&self) -> &'static str {
        "metal-sdpa"
    }

    fn cpu_fwd(
        &self,
        _s1: &CpuStorage,
        _l1: &Layout,
        _s2: &CpuStorage,
        _l2: &Layout,
        _s3: &CpuStorage,
        _l3: &Layout,
    ) -> Result<(CpuStorage, Shape)> {
        crate::bail!("SDPA has no cpu impl")
    }

    #[cfg(feature = "metal")]
    fn metal_fwd(
        &self,
        q: &crate::core::MetalStorage,
        q_l: &Layout,
        k: &crate::core::MetalStorage,
        k_l: &Layout,
        v: &crate::core::MetalStorage,
        v_l: &Layout,
    ) -> Result<(crate::core::MetalStorage, Shape)> {
        use crate::core::backend::BackendStorage;
        use crate::metal_kernels::SdpaDType;

        let device = q.device();

        let out_dims = vec![q_l.dim(0)?, q_l.dim(1)?, q_l.dim(2)?, v_l.dim(3)?];
        let elem_count: usize = out_dims.iter().product();

        let output = device.new_buffer(elem_count, q.dtype(), "sdpa_o")?;

        // q,k must have matching emb dim
        if q_l.dim(D::Minus1)? != k_l.dim(D::Minus1)? {
            crate::bail!("`q` and `k` last dims must match");
        }

        // k,v must have matching n kv heads
        if v_l.dim(D::Minus(3))? != k_l.dim(D::Minus(3))? {
            crate::bail!("`k` and `v` head dims must match");
        }

        // n_heads % n_kv_heads == 0; n_heads >= 1, n_kv_heads >= 1.
        if q_l.dim(D::Minus(3))? % k_l.dim(D::Minus(3))? != 0 {
            crate::bail!("query `n_heads` must be a multiple of `n_kv_heads`");
        }

        let k_head = k_l.dim(D::Minus1)?;
        let q_head = q_l.dim(D::Minus1)?;
        let q_seq = q_l.dim(2)?;

        let mut implementation_supports_use_case = q_head == k_head;
        let supported_head_dim =
            q_head == 32 || q_head == 64 || q_head == 96 || q_head == 128 || q_head == 256;

        const SDPA_FULL_THRESHOLD: usize = 2;

        let supports_sdpa_full =
            q_seq >= SDPA_FULL_THRESHOLD && supported_head_dim && q_head == k_head;
        let supports_sdpa_vector = q_seq == 1 && supported_head_dim;

        implementation_supports_use_case &= supports_sdpa_full || supports_sdpa_vector;

        if !supported_head_dim {
            crate::bail!(
                "Meta SDPA does not support q head dim {q_head}: q dims {:?}, k dims {:?}, v dims {:?}.",
                q_l.dims(),
                k_l.dims(),
                v_l.dims()
            );
        }
        if !implementation_supports_use_case {
            crate::bail!(
                "Meta SDPA does not support q dims {:?}, k dims {:?}, v dims {:?}.",
                q_l.dims(),
                k_l.dims(),
                v_l.dims()
            );
        }

        for t in [k.dtype(), v.dtype()] {
            if q.dtype() != t {
                crate::bail!("all q, k, v dtypes must match.");
            }
        }

        let itype = match q.dtype() {
            DType::BF16 => SdpaDType::BF16,
            DType::F16 => SdpaDType::F16,
            DType::F32 => SdpaDType::F32,
            other => crate::bail!("unsupported sdpa type {other:?}"),
        };

        let command_buffer = q.device().command_buffer()?;
        if supports_sdpa_vector {
            // Route to the 2 pass fused attention if the k seqlen is large.
            // https://github.com/ml-explore/mlx/pull/1597
            const TWO_PASS_K_THRESHOLD: usize = 1024;
            if k_l.dim(2)? >= TWO_PASS_K_THRESHOLD {
                let mut intermediate_shape = [
                    &out_dims[0..out_dims.len() - 2],
                    &[crate::metal_kernels::SDPA_2PASS_BLOCKS],
                    &[out_dims[out_dims.len() - 1]],
                ]
                .concat();
                let intermediate = device.new_buffer(
                    intermediate_shape.iter().product::<usize>(),
                    DType::F32,
                    "sdpa_2pass_intermediate",
                )?;
                let _ = intermediate_shape.pop().unwrap();
                let sums = device.new_buffer(
                    intermediate_shape.iter().product::<usize>(),
                    DType::F32,
                    "sdpa_2pass_sums",
                )?;
                let maxs = device.new_buffer(
                    intermediate_shape.iter().product::<usize>(),
                    DType::F32,
                    "sdpa_2pass_maxs",
                )?;

                command_buffer.set_label("vector_attention");
                crate::metal_kernels::call_sdpa_vector_2pass(
                    q.device().device(),
                    &command_buffer,
                    q.device().kernels(),
                    q_l.start_offset(),
                    q_l.dims(),
                    q.buffer(),
                    k_l.start_offset(),
                    k_l.dims(),
                    k_l.stride(),
                    k.buffer(),
                    v_l.start_offset(),
                    v_l.stride(),
                    v.buffer(),
                    &output,
                    &intermediate,
                    &sums,
                    &maxs,
                    self.scale,
                    self.softcapping,
                    itype,
                )
                .map_err(crate::core::Error::wrap)?;
            } else {
                command_buffer.set_label("vector_attention");
                crate::metal_kernels::call_sdpa_vector(
                    q.device().device(),
                    &command_buffer,
                    q.device().kernels(),
                    q_l.start_offset(),
                    q_l.dims(),
                    q.buffer(),
                    k_l.start_offset(),
                    k_l.dims(),
                    k_l.stride(),
                    k.buffer(),
                    v_l.start_offset(),
                    v_l.stride(),
                    v.buffer(),
                    &output,
                    self.scale,
                    self.softcapping,
                    itype,
                )
                .map_err(crate::core::Error::wrap)?;
            }
        } else if supports_sdpa_full {
            if q_l.dim(2)? != k_l.dim(2)? {
                crate::bail!("query and key sequence length must be equal if using full metal sdpa")
            }

            command_buffer.set_label("full_attention");
            crate::metal_kernels::call_sdpa_full(
                q.device().device(),
                &command_buffer,
                q.device().kernels(),
                q_l.start_offset(),
                q_l.dims(),
                q.buffer(),
                k_l.start_offset(),
                k.buffer(),
                v_l.start_offset(),
                v.buffer(),
                &output,
                self.scale,
                self.softcapping,
                itype,
            )
            .map_err(crate::core::Error::wrap)?;
        } else {
            crate::bail!("must be vector or full sdpa kernel");
        }

        let newstorage =
            crate::core::MetalStorage::new(output, device.clone(), elem_count, q.dtype());
        Ok((newstorage, Shape::from_dims(&out_dims)))
    }
}

/// Scaled dot product attention with a fused kernel.
///
/// Computes softmax(qk^T*scale)v.
///
/// **Inputs shapes:**
/// - `q`: (bs, qhead, seq, hidden)
/// - `k`: (bs, kv_head, kv_seq, hidden)
/// - `k`: (bs, kv_head, kv_seq, v_hidden)
/// - `scale` is applied before softmax.
/// - If `softcapping` != 1.0:
///      - Computation is: softmax(tanh(qk^T*scale/cap)*cap)v
///
/// **Output shape:** (bs, qhead, seq, v_hidden)
///
/// **Supported head dims:** 32, 64, 96, 128, 256.
///
/// ## On Metal:
/// - If `seq` == 1:
///     - Use a vectorized kernel
///     - Supports `seq` != `kv_seq` (cross attn. support)
///     - Supports GQA when `qhead` is a multiple of `kv_head`
/// - Otherwise:
///     - Use an alternate kernel
///     - Requires `seq` == `kv_seq`
///     - GQA is not supported (requires `qhead` == `kv_head`)
pub fn sdpa(q: &Tensor, k: &Tensor, v: &Tensor, scale: f32, softcapping: f32) -> Result<Tensor> {
    q.apply_op3_no_bwd(k, v, &Sdpa { scale, softcapping })
}
