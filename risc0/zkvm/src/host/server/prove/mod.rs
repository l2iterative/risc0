// Copyright 2024 RISC Zero, Inc.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! Run the zkVM guest and prove its results.

mod dev_mode;
mod exec;
pub(crate) mod loader;
mod plonk;
mod prover_impl;
#[cfg(test)]
mod tests;

use std::rc::Rc;

use anyhow::{anyhow, bail, Result};
use cfg_if::cfg_if;
use risc0_circuit_rv32im::CircuitImpl;
use risc0_core::field::{
    baby_bear::{BabyBear, Elem, ExtElem},
    Elem as _,
};
use risc0_zkp::{
    adapter::CircuitInfo,
    core::digest::DIGEST_WORDS,
    hal::{CircuitHal, Hal},
};
use risc0_zkvm_platform::WORD_SIZE;

use self::{dev_mode::DevModeProver, prover_impl::ProverImpl};
use crate::{
    host::receipt::{CompositeReceipt, InnerReceipt, SegmentReceipt, SuccinctReceipt},
    is_dev_mode, ExecutorEnv, ExecutorImpl, ProverOpts, Receipt, Segment, Session, VerifierContext,
};

/// A ProverServer can execute a given ELF binary and produce a [Receipt]
/// that can be used to verify correct computation.
pub trait ProverServer {
    /// Prove the specified ELF binary.
    fn prove(&self, env: ExecutorEnv<'_>, elf: &[u8]) -> Result<Receipt> {
        self.prove_with_ctx(env, &VerifierContext::default(), elf)
    }

    /// Prove the specified ELF binary using the specified [VerifierContext].
    fn prove_with_ctx(
        &self,
        env: ExecutorEnv<'_>,
        ctx: &VerifierContext,
        elf: &[u8],
    ) -> Result<Receipt> {
        let mut exec = ExecutorImpl::from_elf(env, elf)?;
        let session = exec.run()?;
        self.prove_session(ctx, &session)
    }

    /// Prove the specified [Session].
    fn prove_session(&self, ctx: &VerifierContext, session: &Session) -> Result<Receipt>;

    /// Prove the specified [Segment].
    fn prove_segment(&self, ctx: &VerifierContext, segment: &Segment) -> Result<SegmentReceipt>;

    /// Return the peak memory usage that this [ProverServer] has experienced.
    fn get_peak_memory_usage(&self) -> usize;

    /// Lift a [SegmentReceipt] into a [SuccinctReceipt]
    fn lift(&self, receipt: &SegmentReceipt) -> Result<SuccinctReceipt>;

    /// Join two [SuccinctReceipt] into a [SuccinctReceipt]
    fn join(&self, a: &SuccinctReceipt, b: &SuccinctReceipt) -> Result<SuccinctReceipt>;

    /// Resolve an assumption from a conditional [SuccinctReceipt] by providing a [SuccinctReceipt]
    /// proving the validity of the assumption.
    fn resolve(
        &self,
        conditional: &SuccinctReceipt,
        assumption: &SuccinctReceipt,
    ) -> Result<SuccinctReceipt>;

    /// Convert a [SuccinctReceipt] with a Poseidon hash function that uses a 254-bit field
    fn identity_p254(&self, a: &SuccinctReceipt) -> Result<SuccinctReceipt>;

    /// Compress a [CompositeReceipt] into a single [SuccinctReceipt].
    ///
    /// A [CompositeReceipt] may contain an arbitrary number of receipts assembled into
    /// continuations and compositions. Together, these receipts collectively prove a top-level
    /// [ReceiptClaim](crate::ReceiptClaim). This function compresses all of the constituent receipts of a
    /// [CompositeReceipt] into a single [SuccinctReceipt] that proves the same top-level claim. It
    /// accomplishes this by iterative application of the recursion programs including lift, join,
    /// and resolve.
    fn compress(&self, receipt: &CompositeReceipt) -> Result<SuccinctReceipt> {
        // Compress all receipts in the top-level session into one succinct receipt for the session.
        let continuation_receipt = receipt
            .segments
            .iter()
            .try_fold(
                None,
                |left: Option<SuccinctReceipt>, right: &SegmentReceipt| -> Result<_> {
                    Ok(Some(match left {
                        Some(left) => self.join(&left, &self.lift(right)?)?,
                        None => self.lift(right)?,
                    }))
                },
            )?
            .ok_or(anyhow!(
                "malformed composite receipt has no continuation segment receipts"
            ))?;

        // Compress assumptions and resolve them to get the final succinct receipt.
        receipt.assumptions.iter().try_fold(
            continuation_receipt,
            |conditional: SuccinctReceipt, assumption: &InnerReceipt| match assumption {
                InnerReceipt::Succinct(assumption) => self.resolve(&conditional, assumption),
                InnerReceipt::Composite(assumption) => {
                    self.resolve(&conditional, &self.compress(assumption)?)
                }
                InnerReceipt::Fake { .. } => bail!(
                    "compressing composite receipts with fake receipt assumptions is not supported"
                ),
                InnerReceipt::Groth16(_) => bail!(
                    "compressing composite receipts with Groth16 receipt assumptions is not supported"
                )
            },
        )
    }
}

/// A pair of [Hal] and [CircuitHal].
#[derive(Clone)]
pub struct HalPair<H, C>
where
    H: Hal<Field = BabyBear, Elem = Elem, ExtElem = ExtElem>,
    C: CircuitHal<H>,
{
    /// A [Hal] implementation.
    pub hal: Rc<H>,

    /// An [CircuitHal] implementation.
    pub circuit_hal: Rc<C>,
}

impl Session {
    /// For each segment, call [Segment::prove] and collect the receipts.
    pub fn prove(&self) -> Result<Receipt> {
        let prover = get_prover_server(&ProverOpts::default())?;
        prover.prove_session(&VerifierContext::default(), self)
    }
}

impl Segment {
    /// Call the ZKP system to produce a [SegmentReceipt].
    pub fn prove(&self, ctx: &VerifierContext) -> Result<SegmentReceipt> {
        let prover = get_prover_server(&ProverOpts::default())?;
        prover.prove_segment(ctx, self)
    }

    fn prepare_globals(&self) -> Result<Vec<Elem>> {
        let mut io = vec![Elem::INVALID; CircuitImpl::OUTPUT_SIZE];
        tracing::debug!("run> pc: 0x{:08x}", self.pre_image.pc);

        // initialize Input
        let mut offset = 0;
        for i in 0..DIGEST_WORDS * WORD_SIZE {
            io[offset + i] = Elem::ZERO;
        }
        offset += DIGEST_WORDS * WORD_SIZE;

        // initialize PC
        let pc_bytes = self.pre_image.pc.to_le_bytes();
        for i in 0..WORD_SIZE {
            io[offset + i] = (pc_bytes[i] as u32).into();
        }
        offset += WORD_SIZE;

        // initialize ImageID
        let merkle_root = self.pre_image.compute_root_hash()?;
        let merkle_root = merkle_root.as_words();
        for i in 0..DIGEST_WORDS {
            let bytes = merkle_root[i].to_le_bytes();
            for j in 0..WORD_SIZE {
                io[offset + i * WORD_SIZE + j] = (bytes[j] as u32).into();
            }
        }

        Ok(io)
    }
}

#[cfg(feature = "cuda")]
mod cuda {
    use std::rc::Rc;

    use anyhow::{bail, Result};
    use risc0_circuit_rv32im::cuda::{CudaCircuitHalPoseidon, CudaCircuitHalSha256};
    use risc0_zkp::hal::cuda::{CudaHalPoseidon, CudaHalSha256};

    use super::{HalPair, ProverImpl, ProverServer};
    use crate::ProverOpts;

    pub fn get_prover_server(opts: &ProverOpts) -> Result<Rc<dyn ProverServer>> {
        match opts.hashfn.as_str() {
            "sha-256" => {
                let hal = Rc::new(CudaHalSha256::new());
                let circuit_hal = Rc::new(CudaCircuitHalSha256::new(hal.clone()));
                Ok(Rc::new(ProverImpl::new(
                    "cuda",
                    HalPair { hal, circuit_hal },
                )))
            }
            "poseidon" => {
                let hal = Rc::new(CudaHalPoseidon::new());
                let circuit_hal = Rc::new(CudaCircuitHalPoseidon::new(hal.clone()));
                Ok(Rc::new(ProverImpl::new(
                    "cuda",
                    HalPair { hal, circuit_hal },
                )))
            }
            _ => bail!("Unsupported hashfn: {}", opts.hashfn),
        }
    }
}

#[cfg(feature = "metal")]
mod metal {
    use std::rc::Rc;

    use anyhow::{bail, Result};
    use risc0_circuit_rv32im::metal::MetalCircuitHal;
    use risc0_zkp::hal::metal::{
        MetalHalPoseidon, MetalHalSha256, MetalHashPoseidon, MetalHashSha256,
    };

    use super::{HalPair, ProverImpl, ProverServer};
    use crate::ProverOpts;

    pub fn get_prover_server(opts: &ProverOpts) -> Result<Rc<dyn ProverServer>> {
        match opts.hashfn.as_str() {
            "sha-256" => {
                let hal = Rc::new(MetalHalSha256::new());
                let circuit_hal = Rc::new(MetalCircuitHal::<MetalHashSha256>::new(hal.clone()));
                Ok(Rc::new(ProverImpl::new(
                    "metal",
                    HalPair { hal, circuit_hal },
                )))
            }
            "poseidon" => {
                let hal = Rc::new(MetalHalPoseidon::new());
                let circuit_hal = Rc::new(MetalCircuitHal::<MetalHashPoseidon>::new(hal.clone()));
                Ok(Rc::new(ProverImpl::new(
                    "metal",
                    HalPair { hal, circuit_hal },
                )))
            }
            _ => bail!("Unsupported hashfn: {}", opts.hashfn),
        }
    }
}

#[allow(dead_code)]
mod cpu {
    use std::rc::Rc;

    use anyhow::{bail, Result};
    use risc0_circuit_rv32im::cpu::CpuCircuitHal;
    use risc0_zkp::{
        core::hash::{poseidon::PoseidonHashSuite, sha::Sha256HashSuite},
        hal::cpu::CpuHal,
    };

    use super::{HalPair, ProverImpl, ProverServer};
    use crate::{host::CIRCUIT, ProverOpts};

    pub fn get_prover_server(opts: &ProverOpts) -> Result<Rc<dyn ProverServer>> {
        let suite = match opts.hashfn.as_str() {
            "sha-256" => Sha256HashSuite::new_suite(),
            "poseidon" => PoseidonHashSuite::new_suite(),
            _ => bail!("Unsupported hashfn: {}", opts.hashfn),
        };
        let hal = Rc::new(CpuHal::new(suite));
        let circuit_hal = Rc::new(CpuCircuitHal::new(&CIRCUIT));
        let hal_pair = HalPair { hal, circuit_hal };
        Ok(Rc::new(ProverImpl::new("cpu", hal_pair)))
    }
}

/// Select a [ProverServer] based on the specified [ProverOpts] and currently
/// compiled features.
pub fn get_prover_server(opts: &ProverOpts) -> Result<Rc<dyn ProverServer>> {
    if is_dev_mode() {
        eprintln!("WARNING: proving in dev mode. This will not generate valid, secure proofs.");
        return Ok(Rc::new(DevModeProver));
    }

    cfg_if! {
        if #[cfg(feature = "cuda")] {
            cuda::get_prover_server(opts)
        } else if #[cfg(feature = "metal")] {
            metal::get_prover_server(opts)
        } else {
            cpu::get_prover_server(opts)
        }
    }
}
