use diffusion_rs_common::core::cuda::cudarc::driver::DevicePtr;
use float8::F8E4M3;
use std::ffi::c_int;

use diffusion_rs_common::core::backend::BackendStorage;
use diffusion_rs_common::core::cuda_backend::WrapErr;
use diffusion_rs_common::core::{
    CpuStorage, DType, Device, Layout, Result, Shape, Storage, Tensor,
};
use half::{bf16, f16};
use std::sync::Arc;

use super::matmul::{Activation, CudaBlasLT, Matmul, MatmulConfig, OutSlice};
use super::F8MatmulOutType;

#[derive(Debug, Clone)]
pub struct CublasLt(Arc<CudaBlasLT>);

impl CublasLt {
    pub fn new(device: &Device) -> Result<Self> {
        let dev = match device {
            Device::Cuda(d) => d,
            _ => diffusion_rs_common::bail!("`device` must be a `cuda` device"),
        };

        let inner = CudaBlasLT::new(dev.cuda_device()).unwrap();

        Ok(Self(Arc::new(inner)))
    }
}

pub struct CublasLTBatchMatmul {
    pub cublaslt: Arc<CudaBlasLT>,
    pub act: Option<Activation>,
    pub c: Option<Tensor>,
    pub alpha: Option<f32>,
    pub beta: Option<f32>,
}

impl CublasLTBatchMatmul {
    pub fn fwd_f16(
        &self,
        a: &diffusion_rs_common::core::CudaStorage,
        a_l: &Layout,
        b: &diffusion_rs_common::core::CudaStorage,
        b_l: &Layout,
        bias: Option<&diffusion_rs_common::core::CudaStorage>,
        bias_l: Option<&Layout>,
    ) -> Result<(diffusion_rs_common::core::CudaStorage, Shape)> {
        let dev = a.device();

        // Assume TN
        let (batch_size, m, k) = a_l.shape().dims3()?;
        let (b_0, n, b_2) = b_l.shape().dims3()?;

        if b_2 != k {
            diffusion_rs_common::bail!("This layer only supports TN layout");
        }

        if b_0 != batch_size {
            diffusion_rs_common::bail!("`b` must have the same batch size as `a`")
        }

        let lda = k;
        let ldb = k;
        let ldc = m;

        let out_shape = Shape::from((batch_size, n, m));

        let a = a.as_cuda_slice::<f16>()?.slice(a_l.start_offset()..);
        let b = b.as_cuda_slice::<f16>()?.slice(b_l.start_offset()..);

        let bias = if let (Some(bias), Some(bias_l)) = (bias, bias_l) {
            if bias_l.shape().dims1()? != m {
                diffusion_rs_common::bail!("Bias does not have the correct shape");
            }

            Some(bias.as_cuda_slice::<f16>()?.slice(bias_l.start_offset()..))
        } else {
            None
        };

        let (mut out, stride_c) = if let Some(c) = &self.c {
            let (c, c_l) = c.storage_and_layout();
            let c = match &*c {
                Storage::Cuda(storage) => storage.as_cuda_slice::<f16>()?,
                _ => diffusion_rs_common::bail!("`c` must be a cuda tensor"),
            };
            match c_l.contiguous_offsets() {
                Some((o1, o2)) => {
                    if o1 != 0 {
                        diffusion_rs_common::bail!("`c` start offset must be 0");
                    }
                    if o2 != out_shape.elem_count() {
                        diffusion_rs_common::bail!(
                            "`c` end offset must be {}",
                            out_shape.elem_count()
                        )
                    }
                }
                None => diffusion_rs_common::bail!("`c` has to be contiguous"),
            };

            if c_l.shape().dims3()? != (batch_size, n, m) {
                diffusion_rs_common::bail!("`c` does not have the correct shape");
            }

            // Set beta to 0.0 if it is not set
            (c.clone(), c_l.stride()[0])
        } else {
            // Allocate out tensor
            (
                unsafe { dev.alloc::<f16>(out_shape.elem_count()).w()? },
                (n * m),
            )
        };

        let config = MatmulConfig {
            transa: true,
            transb: false,
            m: m as u64,
            n: n as u64,
            k: k as u64,
            alpha: self.alpha.unwrap_or(1.0),
            lda: lda as i64,
            ldb: ldb as i64,
            beta: self.beta.unwrap_or(0.0),
            ldc: ldc as i64,
            stride_a: Some(a_l.stride()[0] as i64),
            stride_b: Some(b_l.stride()[0] as i64),
            stride_c: Some(stride_c as i64),
            stride_bias: None,
            batch_size: Some(c_int::try_from(batch_size)?),
        };

        unsafe {
            self.cublaslt
                .matmul(config, &a, &b, &mut out, bias.as_ref(), self.act.as_ref())
                .map_err(|e| diffusion_rs_common::core::Error::Cuda(Box::new(e)))?;
        }

        let out = diffusion_rs_common::core::CudaStorage::wrap_cuda_slice(out, dev.clone());

        Ok((out, out_shape))
    }

    pub fn fwd_bf16(
        &self,
        a: &diffusion_rs_common::core::CudaStorage,
        a_l: &Layout,
        b: &diffusion_rs_common::core::CudaStorage,
        b_l: &Layout,
        bias: Option<&diffusion_rs_common::core::CudaStorage>,
        bias_l: Option<&Layout>,
    ) -> Result<(diffusion_rs_common::core::CudaStorage, Shape)> {
        let dev = a.device();

        // Assume TN
        let (batch_size, m, k) = a_l.shape().dims3()?;
        let (b_0, n, b_2) = b_l.shape().dims3()?;

        if b_2 != k {
            diffusion_rs_common::bail!("This layer only supports TN layout");
        }

        if b_0 != batch_size {
            diffusion_rs_common::bail!("`b` must have the same batch size as `a`")
        }

        let lda = k;
        let ldb = k;
        let ldc = m;

        let out_shape = Shape::from((batch_size, n, m));

        let a = a.as_cuda_slice::<bf16>()?.slice(a_l.start_offset()..);
        let b = b.as_cuda_slice::<bf16>()?.slice(b_l.start_offset()..);

        let bias = if let (Some(bias), Some(bias_l)) = (bias, bias_l) {
            if bias_l.shape().dims1()? != m {
                diffusion_rs_common::bail!("Bias does not have the correct shape");
            }

            Some(bias.as_cuda_slice::<bf16>()?.slice(bias_l.start_offset()..))
        } else {
            None
        };

        let (mut out, stride_c) = if let Some(c) = &self.c {
            let (c, c_l) = c.storage_and_layout();
            let c = match &*c {
                Storage::Cuda(storage) => storage.as_cuda_slice::<bf16>()?,
                _ => diffusion_rs_common::bail!("`c` must be a cuda tensor"),
            };
            match c_l.contiguous_offsets() {
                Some((o1, o2)) => {
                    if o1 != 0 {
                        diffusion_rs_common::bail!("`c` start offset must be 0");
                    }
                    if o2 != out_shape.elem_count() {
                        diffusion_rs_common::bail!(
                            "`c` end offset must be {}",
                            out_shape.elem_count()
                        )
                    }
                }
                None => diffusion_rs_common::bail!("`c` has to be contiguous"),
            };

            if c_l.shape().dims3()? != (batch_size, n, m) {
                diffusion_rs_common::bail!("`c` does not have the correct shape");
            }

            // Set beta to 0.0 if it is not set
            (c.clone(), c_l.stride()[0])
        } else {
            // Allocate out tensor
            (
                unsafe { dev.alloc::<bf16>(out_shape.elem_count()).w()? },
                (n * m),
            )
        };

        let config = MatmulConfig {
            transa: true,
            transb: false,
            m: m as u64,
            n: n as u64,
            k: k as u64,
            alpha: self.alpha.unwrap_or(1.0),
            lda: lda as i64,
            ldb: ldb as i64,
            beta: self.beta.unwrap_or(0.0),
            ldc: ldc as i64,
            stride_a: Some(a_l.stride()[0] as i64),
            stride_b: Some(b_l.stride()[0] as i64),
            stride_c: Some(stride_c as i64),
            stride_bias: None,
            batch_size: Some(c_int::try_from(batch_size)?),
        };

        unsafe {
            self.cublaslt
                .matmul(config, &a, &b, &mut out, bias.as_ref(), self.act.as_ref())
                .map_err(|e| diffusion_rs_common::core::Error::Cuda(Box::new(e)))?;
        }

        let out = diffusion_rs_common::core::CudaStorage::wrap_cuda_slice(out, dev.clone());

        Ok((out, out_shape))
    }

    pub fn fwd_f32(
        &self,
        a: &diffusion_rs_common::core::CudaStorage,
        a_l: &Layout,
        b: &diffusion_rs_common::core::CudaStorage,
        b_l: &Layout,
        bias: Option<&diffusion_rs_common::core::CudaStorage>,
        bias_l: Option<&Layout>,
    ) -> Result<(diffusion_rs_common::core::CudaStorage, Shape)> {
        let dev = a.device();

        // Assume TN
        let (batch_size, m, k) = a_l.shape().dims3()?;
        let (b_0, n, b_2) = b_l.shape().dims3()?;

        if b_2 != k {
            diffusion_rs_common::bail!("This layer only supports TN layout");
        }

        if b_0 != batch_size {
            diffusion_rs_common::bail!("`b` must have the same batch size as `a`")
        }

        let lda = k;
        let ldb = k;
        let ldc = m;

        let out_shape = Shape::from((batch_size, n, m));

        let a = a.as_cuda_slice::<f32>()?.slice(a_l.start_offset()..);
        let b = b.as_cuda_slice::<f32>()?.slice(b_l.start_offset()..);

        let bias = if let (Some(bias), Some(bias_l)) = (bias, bias_l) {
            if bias_l.shape().dims1()? != m {
                diffusion_rs_common::bail!("Bias does not have the correct shape");
            }

            Some(bias.as_cuda_slice::<f32>()?.slice(bias_l.start_offset()..))
        } else {
            None
        };

        let (mut out, stride_c) = if let Some(c) = &self.c {
            let (c, c_l) = c.storage_and_layout();
            let c = match &*c {
                Storage::Cuda(storage) => storage.as_cuda_slice::<f32>()?,
                _ => diffusion_rs_common::bail!("`c` must be a cuda tensor"),
            };
            match c_l.contiguous_offsets() {
                Some((o1, o2)) => {
                    if o1 != 0 {
                        diffusion_rs_common::bail!("`c` start offset must be 0");
                    }
                    if o2 != out_shape.elem_count() {
                        diffusion_rs_common::bail!(
                            "`c` end offset must be {}",
                            out_shape.elem_count()
                        )
                    }
                }
                None => diffusion_rs_common::bail!("`c` has to be contiguous"),
            };

            if c_l.shape().dims3()? != (batch_size, n, m) {
                diffusion_rs_common::bail!("`c` does not have the correct shape");
            }

            // Set beta to 0.0 if it is not set
            (c.clone(), c_l.stride()[0])
        } else {
            // Allocate out tensor
            (
                unsafe { dev.alloc::<f32>(out_shape.elem_count()).w()? },
                (n * m),
            )
        };

        let config = MatmulConfig {
            transa: true,
            transb: false,
            m: m as u64,
            n: n as u64,
            k: k as u64,
            alpha: self.alpha.unwrap_or(1.0),
            lda: lda as i64,
            ldb: ldb as i64,
            beta: self.beta.unwrap_or(0.0),
            ldc: ldc as i64,
            stride_a: Some(a_l.stride()[0] as i64),
            stride_b: Some(b_l.stride()[0] as i64),
            stride_c: Some(stride_c as i64),
            stride_bias: None,
            batch_size: Some(c_int::try_from(batch_size)?),
        };

        unsafe {
            self.cublaslt
                .matmul(config, &a, &b, &mut out, bias.as_ref(), self.act.as_ref())
                .map_err(|e| diffusion_rs_common::core::Error::Cuda(Box::new(e)))?;
        }

        let out = diffusion_rs_common::core::CudaStorage::wrap_cuda_slice(out, dev.clone());

        Ok((out, out_shape))
    }
}

impl diffusion_rs_common::core::CustomOp2 for CublasLTBatchMatmul {
    fn name(&self) -> &'static str {
        "cublaslt-batch-matmul"
    }

    fn cpu_fwd(
        &self,
        _: &CpuStorage,
        _: &Layout,
        _: &CpuStorage,
        _: &Layout,
    ) -> Result<(CpuStorage, Shape)> {
        diffusion_rs_common::bail!("no cpu support for cublaslt-batch-matmul")
    }

    fn cuda_fwd(
        &self,
        a: &diffusion_rs_common::core::CudaStorage,
        a_l: &Layout,
        b: &diffusion_rs_common::core::CudaStorage,
        b_l: &Layout,
    ) -> Result<(diffusion_rs_common::core::CudaStorage, Shape)> {
        match a.dtype() {
            diffusion_rs_common::core::DType::F16 => self.fwd_f16(a, a_l, b, b_l, None, None),
            diffusion_rs_common::core::DType::BF16 => self.fwd_bf16(a, a_l, b, b_l, None, None),
            diffusion_rs_common::core::DType::F32 => self.fwd_f32(a, a_l, b, b_l, None, None),
            dt => {
                diffusion_rs_common::bail!(
                    "cublaslt-batch-matmul is only supported for f16/bf16/f32 ({dt:?})"
                )
            }
        }
    }
}

impl diffusion_rs_common::core::CustomOp3 for CublasLTBatchMatmul {
    fn name(&self) -> &'static str {
        "cublaslt-batch-matmul-add"
    }

    fn cpu_fwd(
        &self,
        _: &CpuStorage,
        _: &Layout,
        _: &CpuStorage,
        _: &Layout,
        _: &CpuStorage,
        _: &Layout,
    ) -> Result<(CpuStorage, Shape)> {
        diffusion_rs_common::bail!("no cpu support for cublaslt-batch-matmul-add")
    }

    fn cuda_fwd(
        &self,
        a: &diffusion_rs_common::core::CudaStorage,
        a_l: &Layout,
        b: &diffusion_rs_common::core::CudaStorage,
        b_l: &Layout,
        bias: &diffusion_rs_common::core::CudaStorage,
        bias_l: &Layout,
    ) -> Result<(diffusion_rs_common::core::CudaStorage, Shape)> {
        match a.dtype() {
            diffusion_rs_common::core::DType::F16 => {
                self.fwd_f16(a, a_l, b, b_l, Some(bias), Some(bias_l))
            }
            diffusion_rs_common::core::DType::BF16 => {
                self.fwd_bf16(a, a_l, b, b_l, Some(bias), Some(bias_l))
            }
            diffusion_rs_common::core::DType::F32 => {
                self.fwd_f32(a, a_l, b, b_l, Some(bias), Some(bias_l))
            }
            dt => diffusion_rs_common::bail!(
                "cublaslt-batch-matmul-add is only supported for f16/bf16/f32 ({dt:?})"
            ),
        }
    }
}

/// Fused batch matmul + add + Relu/Gelu activation using CublasLt
///
/// # Arguments
///
/// * `a` - Input tensor of size BxMxK
/// * `b` - Input tensor of size BxNxK
/// * `out` - Optional Output tensor of size BxNxK.
///           If set and beta != 0, will be added to the end result of A*B before `act`
/// * `alpha` - Optional scaling factor for A*B
/// * `beta` - Optional scaling factor for C
/// * `bias` - Optional bias tensor of size M
/// * `act` - Optional Gelu or Relu activation. If set, will be added to the end result
/// * `cublaslt` - CublasLt handle
///
/// The resulting tensor is of shape NxM
#[allow(clippy::too_many_arguments)]
pub fn fused_batch_matmul(
    a: &Tensor,
    b: &Tensor,
    out: Option<&Tensor>,
    alpha: Option<f32>,
    beta: Option<f32>,
    bias: Option<&Tensor>,
    act: Option<Activation>,
    cublaslt: CublasLt,
) -> Result<Tensor> {
    let op = CublasLTBatchMatmul {
        act,
        cublaslt: cublaslt.0,
        c: out.cloned(),
        alpha,
        beta,
    };

    if let Some(bias) = bias {
        a.apply_op3(b, bias, op)
    } else {
        a.apply_op2(b, op)
    }
}
