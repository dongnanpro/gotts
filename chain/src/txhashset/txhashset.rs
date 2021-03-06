// Copyright 2018 The Grin Developers
// Modifications Copyright 2019 The Gotts Developers
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

//! Utility structs to handle the 3 MMRs (outputI, outputII,
//! kernel) along the overall header MMR conveniently and transactionally.

use crate::core::core::committed::Committed;
use crate::core::core::hash::{Hash, Hashed};
use crate::core::core::merkle_proof::MerkleProof;
use crate::core::core::pmmr::{self, Backend, ReadonlyPMMR, RewindablePMMR, PMMR};
use crate::core::core::{
	Block, BlockHeader, Input, Output, OutputFeatures, OutputI, OutputII, OutputIdentifier,
	TxKernel,
};
use crate::core::ser::{PMMRIndexHashable, PMMRable};
use crate::error::{Error, ErrorKind};
use crate::store::{Batch, ChainStore};
use crate::txhashset::{RewindableKernelView, UTXOView};
use crate::types::{
	OutputFeaturePosHeight, OutputMMRPosition, Tip, TxHashSetRoots, TxHashsetWriteStatus,
};
use crate::util::secp::pedersen::Commitment;
use crate::util::{file, secp_static, zip};
use croaring::Bitmap;
use gotts_store;
use gotts_store::pmmr::{clean_files_by_prefix, PMMRBackend};
use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

const TXHASHSET_SUBDIR: &'static str = "txhashset";

const OUTPUT_I_SUBDIR: &'static str = "outputI";
const OUTPUT_II_SUBDIR: &'static str = "outputII";
const KERNEL_SUBDIR: &'static str = "kernel";

const TXHASHSET_ZIP: &'static str = "txhashset_snapshot";

/// Convenience wrapper around a single prunable MMR backend.
pub struct PMMRHandle<T: PMMRable> {
	/// The backend storage for the MMR.
	pub backend: PMMRBackend<T>,
	/// The last position accessible via this MMR handle (backend may continue out beyond this).
	pub last_pos: u64,
}

impl<T: PMMRable> PMMRHandle<T> {
	/// Constructor to create a PMMR handle from an existing directory structure on disk.
	/// Creates the backend files as necessary if they do not already exist.
	pub fn new(
		root_dir: &str,
		sub_dir: &str,
		file_name: &str,
		prunable: bool,
		fixed_size: bool,
		header: Option<&BlockHeader>,
	) -> Result<PMMRHandle<T>, Error> {
		let path = Path::new(root_dir).join(sub_dir).join(file_name);
		fs::create_dir_all(path.clone())?;
		let path_str = path.to_str().ok_or(Error::from(ErrorKind::Other(
			"invalid file path".to_owned(),
		)))?;
		let backend = PMMRBackend::new(path_str.to_string(), prunable, fixed_size, header)?;
		let last_pos = backend.unpruned_size();
		Ok(PMMRHandle { backend, last_pos })
	}
}

impl PMMRHandle<BlockHeader> {
	/// Get the header hash at the specified height based on the current header MMR state.
	pub fn get_header_hash_by_height(&self, height: u64) -> Result<Hash, Error> {
		let pos = pmmr::insertion_to_pmmr_index(height + 1);
		let header_pmmr = ReadonlyPMMR::at(&self.backend, self.last_pos);
		if let Some(entry) = header_pmmr.get_data(pos) {
			Ok(entry.hash())
		} else {
			Err(ErrorKind::Other(format!("get header hash by height")).into())
		}
	}
}

/// An easy to manipulate structure holding the 3 sum trees necessary to
/// validate blocks and capturing the Output set, the range proofs and the
/// kernels. Also handles the index of Commitments to positions in the
/// output and range proof pmmr trees.
///
/// Note that the index is never authoritative, only the trees are
/// guaranteed to indicate whether an output is spent or not. The index
/// may have commitments that have already been spent, even with
/// pruning enabled.
pub struct TxHashSet {
	output_i_pmmr_h: PMMRHandle<OutputI>,
	output_ii_pmmr_h: PMMRHandle<OutputII>,
	kernel_pmmr_h: PMMRHandle<TxKernel>,

	// chain store used as index of commitments to MMR positions
	commit_index: Arc<ChainStore>,
}

impl TxHashSet {
	/// Open an existing or new set of backends for the TxHashSet
	pub fn open(
		root_dir: String,
		commit_index: Arc<ChainStore>,
		header: Option<&BlockHeader>,
	) -> Result<TxHashSet, Error> {
		Ok(TxHashSet {
			output_i_pmmr_h: PMMRHandle::new(
				&root_dir,
				TXHASHSET_SUBDIR,
				OUTPUT_I_SUBDIR,
				true,
				true,
				header,
			)?,
			output_ii_pmmr_h: PMMRHandle::new(
				&root_dir,
				TXHASHSET_SUBDIR,
				OUTPUT_II_SUBDIR,
				true,
				true,
				header,
			)?,
			kernel_pmmr_h: PMMRHandle::new(
				&root_dir,
				TXHASHSET_SUBDIR,
				KERNEL_SUBDIR,
				false, // not prunable
				false, // variable size kernel data file
				None,
			)?,
			commit_index,
		})
	}

	/// Close all backend file handles
	pub fn release_backend_files(&mut self) {
		self.output_i_pmmr_h.backend.release_files();
		self.output_ii_pmmr_h.backend.release_files();
		self.kernel_pmmr_h.backend.release_files();
	}

	/// Check if an output is unspent.
	/// We look in the index to find the output MMR pos.
	/// Then we check the entry in the output MMR and confirm the hash matches.
	pub fn is_unspent(&self, output_commit: &Commitment) -> Result<OutputMMRPosition, Error> {
		match self.commit_index.get_output_pos_height(output_commit) {
			Ok(ofph) => {
				let (hash, output) = match ofph.features {
					OutputFeatures::Plain | OutputFeatures::Coinbase => {
						let output_pmmr: ReadonlyPMMR<'_, OutputI, _> = ReadonlyPMMR::at(
							&self.output_i_pmmr_h.backend,
							self.output_i_pmmr_h.last_pos,
						);
						if let Some(output) = output_pmmr.get_data(ofph.position) {
							(output_pmmr.get_hash(ofph.position), output.into_output())
						} else {
							return Err(ErrorKind::OutputNotFound.into());
						}
					}
					OutputFeatures::SigLocked => {
						let output_pmmr: ReadonlyPMMR<'_, OutputII, _> = ReadonlyPMMR::at(
							&self.output_ii_pmmr_h.backend,
							self.output_ii_pmmr_h.last_pos,
						);
						if let Some(output) = output_pmmr.get_data(ofph.position) {
							(output_pmmr.get_hash(ofph.position), output.into_output())
						} else {
							return Err(ErrorKind::OutputNotFound.into());
						}
					}
				};
				if let Some(hash) = hash {
					if hash == output.hash_with_index(ofph.position - 1)
						&& *output_commit == output.commit
					{
						Ok(OutputMMRPosition {
							output_mmr_hash: hash,
							position: ofph.position,
							height: ofph.height,
							features: output.features.as_flag(),
						})
					} else {
						Err(ErrorKind::TxHashSetErr(format!("txhashset hash mismatch")).into())
					}
				} else {
					Err(ErrorKind::OutputNotFound.into())
				}
			}
			Err(gotts_store::Error::NotFoundErr(_)) => Err(ErrorKind::OutputNotFound.into()),
			Err(e) => Err(ErrorKind::StoreErr(e, format!("txhashset unspent check")).into()),
		}
	}

	/// returns the last N nodes inserted into the tree (i.e. the 'bottom'
	/// nodes at level 0
	pub fn last_n_output_i(&self, distance: u64) -> Vec<(Hash, OutputI)> {
		ReadonlyPMMR::at(&self.output_i_pmmr_h.backend, self.output_i_pmmr_h.last_pos)
			.get_last_n_insertions(distance)
	}

	/// as above, for OutputII
	pub fn last_n_output_ii(&self, distance: u64) -> Vec<(Hash, OutputII)> {
		ReadonlyPMMR::at(
			&self.output_ii_pmmr_h.backend,
			self.output_ii_pmmr_h.last_pos,
		)
		.get_last_n_insertions(distance)
	}

	/// as above, for kernels
	pub fn last_n_kernel(&self, distance: u64) -> Vec<(Hash, TxKernel)> {
		ReadonlyPMMR::at(&self.kernel_pmmr_h.backend, self.kernel_pmmr_h.last_pos)
			.get_last_n_insertions(distance)
	}

	/// Convenience function to query the db for a header by its hash.
	pub fn get_block_header(&self, hash: &Hash) -> Result<BlockHeader, Error> {
		Ok(self.commit_index.get_block_header(&hash)?)
	}

	/// returns outputs from the given insertion (leaf) index up to the
	/// specified limit. Also returns the last index actually populated
	pub fn outputs_i_by_insertion_index(
		&self,
		start_index: u64,
		max_count: u64,
	) -> (u64, Vec<OutputI>) {
		ReadonlyPMMR::at(&self.output_i_pmmr_h.backend, self.output_i_pmmr_h.last_pos)
			.elements_from_insertion_index(start_index, max_count)
	}

	/// returns outputs from the given insertion (leaf) index up to the
	/// specified limit. Also returns the last index actually populated
	pub fn outputs_ii_by_insertion_index(
		&self,
		start_index: u64,
		max_count: u64,
	) -> (u64, Vec<OutputII>) {
		ReadonlyPMMR::at(
			&self.output_ii_pmmr_h.backend,
			self.output_ii_pmmr_h.last_pos,
		)
		.elements_from_insertion_index(start_index, max_count)
	}

	/// returns output from the given mmr position
	pub fn output_i_by_position(&self, position: u64) -> Option<OutputI> {
		ReadonlyPMMR::at(&self.output_i_pmmr_h.backend, self.output_i_pmmr_h.last_pos)
			.get_data(position)
	}

	/// returns output from the given mmr position
	pub fn output_ii_by_position(&self, position: u64) -> Option<OutputII> {
		ReadonlyPMMR::at(
			&self.output_ii_pmmr_h.backend,
			self.output_ii_pmmr_h.last_pos,
		)
		.get_data(position)
	}

	/// highest output insertion index available
	pub fn highest_output_i_insertion_index(&self) -> u64 {
		pmmr::n_leaves(self.output_i_pmmr_h.last_pos)
	}

	/// highest output insertion index available
	pub fn highest_output_ii_insertion_index(&self) -> u64 {
		pmmr::n_leaves(self.output_ii_pmmr_h.last_pos)
	}

	/// Find a kernel with a given excess. Work backwards from `max_index` to `min_index`
	pub fn find_kernel(
		&self,
		excess: &Commitment,
		min_index: Option<u64>,
		max_index: Option<u64>,
	) -> Option<(TxKernel, u64)> {
		let min_index = min_index.unwrap_or(1);
		let max_index = max_index.unwrap_or(self.kernel_pmmr_h.last_pos);

		let pmmr = ReadonlyPMMR::at(&self.kernel_pmmr_h.backend, self.kernel_pmmr_h.last_pos);
		let mut index = max_index + 1;
		while index > min_index {
			index -= 1;
			if let Some(kernel) = pmmr.get_data(index) {
				if &kernel.excess == excess {
					return Some((kernel, index));
				}
			}
		}
		None
	}

	/// Returns a tx kernel from the given insertion (leaf) index.
	pub fn txkernel_by_insertion_index(&self, index: u64) -> Option<TxKernel> {
		ReadonlyPMMR::at(&self.kernel_pmmr_h.backend, self.kernel_pmmr_h.last_pos).get_data(index)
	}

	/// Get MMR roots.
	pub fn roots(&self) -> TxHashSetRoots {
		let output_i_pmmr =
			ReadonlyPMMR::at(&self.output_i_pmmr_h.backend, self.output_i_pmmr_h.last_pos);
		let output_ii_pmmr = ReadonlyPMMR::at(
			&self.output_ii_pmmr_h.backend,
			self.output_ii_pmmr_h.last_pos,
		);
		let kernel_pmmr =
			ReadonlyPMMR::at(&self.kernel_pmmr_h.backend, self.kernel_pmmr_h.last_pos);

		TxHashSetRoots {
			output_i_root: output_i_pmmr.root(),
			output_ii_root: output_ii_pmmr.root(),
			kernel_root: kernel_pmmr.root(),
		}
	}

	/// Return Commit's MMR position and block height
	pub fn get_output_pos_height(
		&self,
		commit: &Commitment,
	) -> Result<OutputFeaturePosHeight, Error> {
		Ok(self.commit_index.get_output_pos_height(&commit)?)
	}

	/// build a new merkle proof for the given position.
	pub fn merkle_proof(&mut self, output_id: &OutputIdentifier) -> Result<MerkleProof, Error> {
		let pos = self.commit_index.get_output_pos(&output_id.commit)?;
		match output_id.features {
			OutputFeatures::Plain | OutputFeatures::Coinbase => PMMR::at(
				&mut self.output_i_pmmr_h.backend,
				self.output_i_pmmr_h.last_pos,
			)
			.merkle_proof(pos)
			.map_err(|_| ErrorKind::MerkleProof.into()),
			OutputFeatures::SigLocked => PMMR::at(
				&mut self.output_ii_pmmr_h.backend,
				self.output_ii_pmmr_h.last_pos,
			)
			.merkle_proof(pos)
			.map_err(|_| ErrorKind::MerkleProof.into()),
		}
	}

	/// Compact the MMR data files and flush the rm logs
	pub fn compact(
		&mut self,
		horizon_header: &BlockHeader,
		batch: &mut Batch<'_>,
	) -> Result<(), Error> {
		debug!("txhashset: starting compaction...");

		let head_header = batch.head_header()?;
		let rewind_rm_pos = input_pos_to_rewind(&horizon_header, &head_header, batch)?;

		debug!("txhashset: check_compact output_i mmr backend...");
		self.output_i_pmmr_h
			.backend
			.check_compact(horizon_header.output_i_mmr_size, &rewind_rm_pos)?;

		debug!("txhashset: check_compact output_ii mmr backend...");
		self.output_ii_pmmr_h
			.backend
			.check_compact(horizon_header.output_ii_mmr_size, &rewind_rm_pos)?;

		debug!("txhashset: ... compaction finished");

		Ok(())
	}

	/// Rebuild the index of block height & MMR positions to the corresponding UTXOs.
	/// This is a costly operation performed only when we receive a full new chain state.
	/// Note: only called by compact and txhashset_write.
	pub fn rebuild_height_pos_index(
		&self,
		header_pmmr: &PMMRHandle<BlockHeader>,
		batch: &mut Batch<'_>,
	) -> Result<(), Error> {
		let now = Instant::now();

		let output_i_pmmr =
			ReadonlyPMMR::at(&self.output_i_pmmr_h.backend, self.output_i_pmmr_h.last_pos);
		let output_ii_pmmr = ReadonlyPMMR::at(
			&self.output_ii_pmmr_h.backend,
			self.output_ii_pmmr_h.last_pos,
		);

		// clear it before rebuilding
		batch.clear_output_pos_height()?;

		let mut outputs_pos: Vec<(OutputIdentifier, u64)> = vec![];

		// search for outputI PMMR
		for pos in output_i_pmmr.leaf_pos_iter() {
			if let Some(out) = output_i_pmmr.get_data(pos) {
				outputs_pos.push((out.id, pos));
			}
		}
		let total_outputs = outputs_pos.len();
		debug!(
			"rebuild_height_pos_index: rebuilding {} outputI outputs position & height...",
			total_outputs
		);

		let max_height = batch.head()?.height;

		let mut i = 0;
		for search_height in 0..max_height {
			let hash = header_pmmr.get_header_hash_by_height(search_height + 1)?;
			let h = batch.get_block_header(&hash)?;
			while i < total_outputs {
				let (id, position) = outputs_pos[i];
				if position > h.output_i_mmr_size {
					break;
				}
				// with special care about the unspent Genesis output
				let height = if position == 1 { 0 } else { h.height };
				batch.save_output_pos_height(
					&id.commit,
					OutputFeaturePosHeight {
						features: id.features,
						position,
						height,
					},
				)?;
				trace!(
					"rebuild_height_pos_index: {:?}",
					(id.commit, position, height)
				);
				i += 1;
			}
		}
		debug!(
			"rebuild_height_pos_index: {} OutputI UTXOs, took {}s",
			total_outputs,
			now.elapsed().as_secs(),
		);
		let now = Instant::now();

		// search for outputII PMMR
		outputs_pos.clear();
		for pos in output_ii_pmmr.leaf_pos_iter() {
			if let Some(out) = output_ii_pmmr.get_data(pos) {
				outputs_pos.push((out.id, pos));
			}
		}
		let total_outputs = outputs_pos.len();
		debug!(
			"rebuild_height_pos_index: rebuilding {} OutputII outputs position & height...",
			total_outputs
		);

		let mut i = 0;
		for search_height in 0..max_height {
			let hash = header_pmmr.get_header_hash_by_height(search_height + 1)?;
			let h = batch.get_block_header(&hash)?;
			while i < total_outputs {
				let (id, position) = outputs_pos[i];
				if position > h.output_ii_mmr_size {
					break;
				}
				let height = h.height;
				batch.save_output_pos_height(
					&id.commit,
					OutputFeaturePosHeight {
						features: id.features,
						position,
						height,
					},
				)?;
				trace!(
					"rebuild_height_pos_index: {:?}",
					(id.commit, position, height)
				);
				i += 1;
			}
		}

		debug!(
			"rebuild_height_pos_index: {} OutputII UTXOs, took {}s",
			total_outputs,
			now.elapsed().as_secs(),
		);
		Ok(())
	}
}

/// Starts a new unit of work to extend (or rewind) the chain with additional
/// blocks. Accepts a closure that will operate within that unit of work.
/// The closure has access to an Extension object that allows the addition
/// of blocks to the txhashset and the checking of the current tree roots.
///
/// The unit of work is always discarded (always rollback) as this is read-only.
pub fn extending_readonly<F, T>(
	handle: &mut PMMRHandle<BlockHeader>,
	trees: &mut TxHashSet,
	inner: F,
) -> Result<T, Error>
where
	F: FnOnce(&mut ExtensionPair<'_>) -> Result<T, Error>,
{
	let commit_index = trees.commit_index.clone();
	let batch = commit_index.batch()?;

	trace!("Starting new txhashset (readonly) extension.");

	let head = batch.head()?;
	let header_head = batch.header_head()?;

	let res = {
		let header_pmmr = PMMR::at(&mut handle.backend, handle.last_pos);
		let mut header_extension = HeaderExtension::new(header_pmmr, &batch, header_head);
		let mut extension = Extension::new(trees, &batch, head);
		let mut extension_pair = ExtensionPair {
			header_extension: &mut header_extension,
			extension: &mut extension,
		};
		inner(&mut extension_pair)
	};

	trace!("Rollbacking txhashset (readonly) extension.");

	handle.backend.discard();

	trees.output_i_pmmr_h.backend.discard();
	trees.output_ii_pmmr_h.backend.discard();
	trees.kernel_pmmr_h.backend.discard();

	trace!("TxHashSet (readonly) extension done.");

	res
}

/// Readonly view on the UTXO set.
/// Based on the current txhashset output_pmmr.
pub fn utxo_view<F, T>(
	handle: &PMMRHandle<BlockHeader>,
	trees: &TxHashSet,
	inner: F,
) -> Result<T, Error>
where
	F: FnOnce(&UTXOView<'_>) -> Result<T, Error>,
{
	let res: Result<T, Error>;
	{
		let output_i_pmmr = ReadonlyPMMR::at(
			&trees.output_i_pmmr_h.backend,
			trees.output_i_pmmr_h.last_pos,
		);
		let output_ii_pmmr = ReadonlyPMMR::at(
			&trees.output_ii_pmmr_h.backend,
			trees.output_ii_pmmr_h.last_pos,
		);
		let header_pmmr = ReadonlyPMMR::at(&handle.backend, handle.last_pos);

		// Create a new batch here to pass into the utxo_view.
		// Discard it (rollback) after we finish with the utxo_view.
		let batch = trees.commit_index.batch()?;
		let utxo = UTXOView::new(output_i_pmmr, output_ii_pmmr, header_pmmr, &batch);
		res = inner(&utxo);
	}
	res
}

/// Rewindable (but still readonly) view on the kernel MMR.
/// The underlying backend is readonly. But we permit the PMMR to be "rewound"
/// via last_pos.
/// We create a new db batch for this view and discard it (rollback)
/// when we are done with the view.
pub fn rewindable_kernel_view<F, T>(trees: &TxHashSet, inner: F) -> Result<T, Error>
where
	F: FnOnce(&mut RewindableKernelView<'_>) -> Result<T, Error>,
{
	let res: Result<T, Error>;
	{
		let kernel_pmmr =
			RewindablePMMR::at(&trees.kernel_pmmr_h.backend, trees.kernel_pmmr_h.last_pos);

		// Create a new batch here to pass into the kernel_view.
		// Discard it (rollback) after we finish with the kernel_view.
		let batch = trees.commit_index.batch()?;
		let header = batch.head_header()?;
		let mut view = RewindableKernelView::new(kernel_pmmr, &batch, header);
		res = inner(&mut view);
	}
	res
}

/// Starts a new unit of work to extend the chain with additional blocks,
/// accepting a closure that will work within that unit of work. The closure
/// has access to an Extension object that allows the addition of blocks to
/// the txhashset and the checking of the current tree roots.
///
/// If the closure returns an error, modifications are canceled and the unit
/// of work is abandoned. Otherwise, the unit of work is permanently applied.
pub fn extending<'a, F, T>(
	header_pmmr: &'a mut PMMRHandle<BlockHeader>,
	trees: &'a mut TxHashSet,
	batch: &'a mut Batch<'_>,
	inner: F,
) -> Result<T, Error>
where
	F: FnOnce(&mut ExtensionPair<'_>) -> Result<T, Error>,
{
	let sizes: (u64, u64, u64);
	let res: Result<T, Error>;
	let rollback: bool;

	let head = batch.head()?;
	let header_head = batch.header_head()?;

	// create a child transaction so if the state is rolled back by itself, all
	// index saving can be undone
	let child_batch = batch.child()?;
	{
		trace!("Starting new txhashset extension.");

		let header_pmmr = PMMR::at(&mut header_pmmr.backend, header_pmmr.last_pos);
		let mut header_extension = HeaderExtension::new(header_pmmr, &child_batch, header_head);
		let mut extension = Extension::new(trees, &child_batch, head);
		let mut extension_pair = ExtensionPair {
			header_extension: &mut header_extension,
			extension: &mut extension,
		};
		res = inner(&mut extension_pair);

		rollback = extension_pair.extension.rollback;
		sizes = extension_pair.extension.sizes();
	}

	// During an extension we do not want to modify the header_extension (and only read from it).
	// So make sure we discard any changes to the header MMR backed.
	header_pmmr.backend.discard();

	match res {
		Err(e) => {
			debug!("Error returned, discarding txhashset extension: {}", e);
			trees.output_i_pmmr_h.backend.discard();
			trees.output_ii_pmmr_h.backend.discard();
			trees.kernel_pmmr_h.backend.discard();
			Err(e)
		}
		Ok(r) => {
			if rollback {
				trace!("Rollbacking txhashset extension. sizes {:?}", sizes);
				trees.output_i_pmmr_h.backend.discard();
				trees.output_ii_pmmr_h.backend.discard();
				trees.kernel_pmmr_h.backend.discard();
			} else {
				trace!("Committing txhashset extension. sizes {:?}", sizes);
				child_batch.commit()?;
				trees.output_i_pmmr_h.backend.sync()?;
				trees.output_ii_pmmr_h.backend.sync()?;
				trees.kernel_pmmr_h.backend.sync()?;
				trees.output_i_pmmr_h.last_pos = sizes.0;
				trees.output_ii_pmmr_h.last_pos = sizes.1;
				trees.kernel_pmmr_h.last_pos = sizes.2;
			}

			trace!("TxHashSet extension done.");
			Ok(r)
		}
	}
}

/// Start a new header MMR unit of work. This MMR tracks the header_head.
/// This MMR can be extended individually beyond the other (output, rangeproof and kernel) MMRs
/// to allow headers to be validated before we receive the full block data.
pub fn header_extending<'a, F, T>(
	handle: &'a mut PMMRHandle<BlockHeader>,
	head: &Tip,
	batch: &'a mut Batch<'_>,
	inner: F,
) -> Result<T, Error>
where
	F: FnOnce(&mut HeaderExtension<'_>) -> Result<T, Error>,
{
	let size: u64;
	let res: Result<T, Error>;
	let rollback: bool;

	// create a child transaction so if the state is rolled back by itself, all
	// index saving can be undone
	let child_batch = batch.child()?;
	{
		let pmmr = PMMR::at(&mut handle.backend, handle.last_pos);
		let mut extension = HeaderExtension::new(pmmr, &child_batch, head.clone());
		res = inner(&mut extension);

		rollback = extension.rollback;
		size = extension.size();
	}

	match res {
		Err(e) => {
			handle.backend.discard();
			Err(e)
		}
		Ok(r) => {
			if rollback {
				handle.backend.discard();
			} else {
				child_batch.commit()?;
				handle.backend.sync()?;
				handle.last_pos = size;
			}
			Ok(r)
		}
	}
}

/// A header extension to allow the header MMR to extend beyond the other MMRs individually.
/// This is to allow headers to be validated against the MMR before we have the full block data.
pub struct HeaderExtension<'a> {
	head: Tip,

	pmmr: PMMR<'a, BlockHeader, PMMRBackend<BlockHeader>>,

	/// Rollback flag.
	rollback: bool,

	/// Batch in which the extension occurs, public so it can be used within
	/// an `extending` closure. Just be careful using it that way as it will
	/// get rolled back with the extension (i.e on a losing fork).
	pub batch: &'a Batch<'a>,
}

impl<'a> HeaderExtension<'a> {
	fn new(
		pmmr: PMMR<'a, BlockHeader, PMMRBackend<BlockHeader>>,
		batch: &'a Batch<'_>,
		head: Tip,
	) -> HeaderExtension<'a> {
		HeaderExtension {
			head,
			pmmr,
			rollback: false,
			batch,
		}
	}

	/// Get the header hash for the specified pos from the underlying MMR backend.
	fn get_header_hash(&self, pos: u64) -> Option<Hash> {
		self.pmmr.get_data(pos).map(|x| x.hash())
	}

	/// The head representing the furthest extent of the current extension.
	pub fn head(&self) -> Tip {
		self.head.clone()
	}

	/// Get the header at the specified height based on the current state of the header extension.
	/// Derives the MMR pos from the height (insertion index) and retrieves the header hash.
	/// Looks the header up in the db by hash.
	pub fn get_header_by_height(&self, height: u64) -> Result<BlockHeader, Error> {
		let pos = pmmr::insertion_to_pmmr_index(height + 1);
		if let Some(hash) = self.get_header_hash(pos) {
			Ok(self.batch.get_block_header(&hash)?)
		} else {
			Err(ErrorKind::Other(format!("get header by height")).into())
		}
	}

	/// Compares the provided header to the header in the header MMR at that height.
	/// If these match we know the header is on the current chain.
	pub fn is_on_current_chain(&self, header: &BlockHeader) -> Result<(), Error> {
		if header.height > self.head.height {
			return Err(ErrorKind::Other(format!("not on current chain, out beyond")).into());
		}
		let chain_header = self.get_header_by_height(header.height)?;
		if chain_header.hash() == header.hash() {
			Ok(())
		} else {
			Err(ErrorKind::Other(format!("not on current chain")).into())
		}
	}

	/// Force the rollback of this extension, no matter the result.
	pub fn force_rollback(&mut self) {
		self.rollback = true;
	}

	/// Apply a new header to the header MMR extension.
	/// This may be either the header MMR or the sync MMR depending on the
	/// extension.
	pub fn apply_header(&mut self, header: &BlockHeader) -> Result<(), Error> {
		self.pmmr.push(header).map_err(&ErrorKind::TxHashSetErr)?;
		self.head = Tip::from_header(header);
		Ok(())
	}

	/// Rewind the header extension to the specified header.
	/// Note the close relationship between header height and insertion index.
	pub fn rewind(&mut self, header: &BlockHeader) -> Result<(), Error> {
		debug!(
			"Rewind header extension to {} at {} from {} at {}",
			header.hash(),
			header.height,
			self.head.hash(),
			self.head.height,
		);

		let header_pos = pmmr::insertion_to_pmmr_index(header.height + 1);
		self.pmmr
			.rewind(header_pos, &Bitmap::create())
			.map_err(&ErrorKind::TxHashSetErr)?;

		// Update our head to reflect the header we rewound to.
		self.head = Tip::from_header(header);

		Ok(())
	}

	/// The size of the header MMR.
	pub fn size(&self) -> u64 {
		self.pmmr.unpruned_size()
	}

	/// The root of the header MMR for convenience.
	pub fn root(&self) -> Result<Hash, Error> {
		Ok(self.pmmr.root().map_err(|_| ErrorKind::InvalidRoot)?)
	}

	/// Validate the prev_root of the header against the root of the current header MMR.
	pub fn validate_root(&self, header: &BlockHeader) -> Result<(), Error> {
		// If we are validating the genesis block then we have no prev_root.
		// So we are done here.
		if header.height == 0 {
			return Ok(());
		}
		if self.root()? != header.prev_root {
			Err(ErrorKind::InvalidRoot.into())
		} else {
			Ok(())
		}
	}
}

/// An extension "pair" consisting of a txhashet extension (outputs, rangeproofs, kernels)
/// and the associated header extension.
pub struct ExtensionPair<'a> {
	/// The header extension.
	pub header_extension: &'a mut HeaderExtension<'a>,
	/// The txhashset extension.
	pub extension: &'a mut Extension<'a>,
}

impl<'a> ExtensionPair<'a> {
	/// Accessor for the batch associated with this extension pair.
	pub fn batch(&mut self) -> &'a Batch<'a> {
		self.extension.batch
	}
}

/// Allows the application of new blocks on top of the sum trees in a
/// reversible manner within a unit of work provided by the `extending`
/// function.
pub struct Extension<'a> {
	head: Tip,

	output_i_pmmr: PMMR<'a, OutputI, PMMRBackend<OutputI>>,
	output_ii_pmmr: PMMR<'a, OutputII, PMMRBackend<OutputII>>,
	kernel_pmmr: PMMR<'a, TxKernel, PMMRBackend<TxKernel>>,

	/// Rollback flag.
	rollback: bool,

	/// Batch in which the extension occurs, public so it can be used within
	/// an `extending` closure. Just be careful using it that way as it will
	/// get rolled back with the extension (i.e on a losing fork).
	pub batch: &'a Batch<'a>,
}

impl<'a> Committed for Extension<'a> {
	fn inputs_committed(&self) -> Vec<Commitment> {
		vec![]
	}

	fn outputs_committed(&self) -> Vec<Commitment> {
		let mut commitments = vec![];
		for pos in self.output_i_pmmr.leaf_pos_iter() {
			if let Some(out) = self.output_i_pmmr.get_data(pos) {
				commitments.push(out.id.commit);
			}
		}
		for pos in self.output_ii_pmmr.leaf_pos_iter() {
			if let Some(out) = self.output_ii_pmmr.get_data(pos) {
				commitments.push(out.id.commit);
			}
		}
		commitments
	}

	fn kernels_committed(&self) -> Vec<Commitment> {
		let mut commitments = vec![];
		for n in 1..self.kernel_pmmr.unpruned_size() + 1 {
			if pmmr::is_leaf(n) {
				if let Some(kernel) = self.kernel_pmmr.get_data(n) {
					commitments.push(kernel.excess());
				}
			}
		}
		commitments
	}
}

impl<'a> Extension<'a> {
	fn new(trees: &'a mut TxHashSet, batch: &'a Batch<'_>, head: Tip) -> Extension<'a> {
		Extension {
			head,
			output_i_pmmr: PMMR::at(
				&mut trees.output_i_pmmr_h.backend,
				trees.output_i_pmmr_h.last_pos,
			),
			output_ii_pmmr: PMMR::at(
				&mut trees.output_ii_pmmr_h.backend,
				trees.output_ii_pmmr_h.last_pos,
			),
			kernel_pmmr: PMMR::at(
				&mut trees.kernel_pmmr_h.backend,
				trees.kernel_pmmr_h.last_pos,
			),
			rollback: false,
			batch,
		}
	}

	/// The head representing the furthest extent of the current extension.
	pub fn head(&self) -> Tip {
		self.head.clone()
	}

	/// Build a view of the current UTXO set based on the output PMMR
	/// and the provided header extension.
	pub fn utxo_view(&'a self, header_ext: &'a HeaderExtension<'a>) -> UTXOView<'a> {
		UTXOView::new(
			self.output_i_pmmr.readonly_pmmr(),
			self.output_ii_pmmr.readonly_pmmr(),
			header_ext.pmmr.readonly_pmmr(),
			self.batch,
		)
	}

	/// Apply a new block to the current txhashet extension (output, rangeproof, kernel MMRs).
	pub fn apply_block(&mut self, b: &Block) -> Result<(), Error> {
		for out in b.outputs() {
			let position = self.apply_output(out)?;
			// Update the (output_pos,height) index for the new output.
			self.batch.save_output_pos_height(
				&out.commitment(),
				OutputFeaturePosHeight {
					features: out.features.as_flag(),
					position,
					height: b.header.height,
				},
			)?;
		}

		for input in b.inputs() {
			self.apply_input(&input)?;
			// todo: Delete the (output_pos,height) index from the spent output.
			//self.batch.delete_output_pos_height(&input.commitment())?;
		}

		for kernel in b.kernels() {
			let position = self.apply_kernel(kernel)?;
			self.batch
				.save_txkernel_pos_height(&kernel.excess, position, b.header.height)?;
		}

		// Update the head of the extension to reflect the block we just applied.
		self.head = Tip::from_header(&b.header);

		Ok(())
	}

	fn apply_input(&mut self, input: &Input) -> Result<(), Error> {
		let commit = input.commitment();
		let ofph_res = self.batch.get_output_pos_height(&commit);
		if let Ok(ofph) = ofph_res {
			// First check this input corresponds to an existing entry in the output MMR.
			let mut is_ok = false;
			match ofph.features {
				OutputFeatures::Plain | OutputFeatures::Coinbase => {
					if let Some(hash) = self.output_i_pmmr.get_hash(ofph.position) {
						if let Some(output) = self.output_i_pmmr.get_data(ofph.position) {
							if hash == output.hash_with_index(ofph.position - 1)
								&& input.commit == output.id.commit
							{
								is_ok = true;
							}
						}
					}
				}
				OutputFeatures::SigLocked => {
					if let Some(hash) = self.output_ii_pmmr.get_hash(ofph.position) {
						if let Some(output) = self.output_ii_pmmr.get_data(ofph.position) {
							if hash == output.hash_with_index(ofph.position - 1)
								&& input.commit == output.id.commit
							{
								is_ok = true;
							}
						}
					}
				}
			}

			if !is_ok {
				return Err(ErrorKind::TxHashSetErr(format!(
					"output pmmr hash not found or mismatch at pos {} for {:?}",
					ofph.position, commit,
				))
				.into());
			}

			// Now prune the output_pmmr and their storage.
			// Input is not valid if we cannot prune successfully (to spend an unspent
			// output).
			let prune_res = match ofph.features {
				OutputFeatures::Plain | OutputFeatures::Coinbase => {
					self.output_i_pmmr.prune(ofph.position)
				}
				OutputFeatures::SigLocked => self.output_ii_pmmr.prune(ofph.position),
			};
			match prune_res {
				Ok(true) => {
					return Ok(());
				}
				Ok(false) => return Err(ErrorKind::AlreadySpent(commit).into()),
				Err(e) => return Err(ErrorKind::TxHashSetErr(e).into()),
			}
		} else {
			return Err(ErrorKind::AlreadySpent(commit).into());
		}
	}

	fn apply_output(&mut self, out: &Output) -> Result<(u64), Error> {
		let commit = out.commitment();

		//todo: think again, can this ensure the commitment unique?
		if let Ok(ofph) = self.batch.get_output_pos_height(&commit) {
			match ofph.features {
				OutputFeatures::Plain | OutputFeatures::Coinbase => {
					if let Some(out_mmr) = self.output_i_pmmr.get_data(ofph.position) {
						if out_mmr.id.commitment() == commit {
							return Err(ErrorKind::DuplicateCommitment(commit).into());
						}
					}
				}
				OutputFeatures::SigLocked => {
					if let Some(out_mmr) = self.output_ii_pmmr.get_data(ofph.position) {
						if out_mmr.id.commitment() == commit {
							return Err(ErrorKind::DuplicateCommitment(commit).into());
						}
					}
				}
			}
		}
		// push the new output to the MMR.
		let output_pos = match out.features.as_flag() {
			OutputFeatures::Plain | OutputFeatures::Coinbase => self
				.output_i_pmmr
				.push(&OutputI::from_output(out)?)
				.map_err(&ErrorKind::TxHashSetErr)?,
			OutputFeatures::SigLocked => self
				.output_ii_pmmr
				.push(&OutputII::from_output(out)?)
				.map_err(&ErrorKind::TxHashSetErr)?,
		};

		Ok(output_pos)
	}

	/// Push kernel onto MMR (hash and data files).
	fn apply_kernel(&mut self, kernel: &TxKernel) -> Result<u64, Error> {
		let kernel_pos = self
			.kernel_pmmr
			.push(kernel)
			.map_err(&ErrorKind::TxHashSetErr)?;
		Ok(kernel_pos)
	}

	/// Build a Merkle proof for the given output and the block
	/// this extension is currently referencing.
	/// Note: this relies on the MMR being stable even after pruning/compaction.
	/// We need the hash of each sibling pos from the pos up to the peak
	/// including the sibling leaf node which may have been removed.
	pub fn merkle_proof(&self, output: &OutputIdentifier) -> Result<MerkleProof, Error> {
		debug!("txhashset: merkle_proof: output: {:?}", output.commit,);
		// then calculate the Merkle Proof based on the known pos
		let ofph = self.batch.get_output_pos_height(&output.commit)?;
		let merkle_proof = match ofph.features {
			OutputFeatures::Plain | OutputFeatures::Coinbase => self
				.output_i_pmmr
				.merkle_proof(ofph.position)
				.map_err(&ErrorKind::TxHashSetErr)?,
			OutputFeatures::SigLocked => self
				.output_ii_pmmr
				.merkle_proof(ofph.position)
				.map_err(&ErrorKind::TxHashSetErr)?,
		};

		Ok(merkle_proof)
	}

	/// Saves a snapshot of the output and rangeproof MMRs to disk.
	/// Specifically - saves a snapshot of the utxo file, tagged with
	/// the block hash as filename suffix.
	/// Needed for fast-sync (utxo file needs to be rewound before sending
	/// across).
	pub fn snapshot(&mut self) -> Result<(), Error> {
		let header = self.batch.get_block_header(&self.head.last_block_h)?;
		self.output_i_pmmr
			.snapshot(&header)
			.map_err(|e| ErrorKind::Other(e))?;
		self.output_ii_pmmr
			.snapshot(&header)
			.map_err(|e| ErrorKind::Other(e))?;
		Ok(())
	}

	/// Rewinds the MMRs to the provided block, rewinding to the last output pos
	/// and last kernel pos of that block.
	pub fn rewind(&mut self, header: &BlockHeader) -> Result<(), Error> {
		debug!(
			"Rewind extension to {} at {} from {} at {}",
			header.hash(),
			header.height,
			self.head.hash(),
			self.head.height
		);

		// We need to build bitmaps of added and removed output positions
		// so we can correctly rewind all operations applied to the output MMR
		// after the position we are rewinding to (these operations will be
		// undone during rewind).
		// Rewound output pos will be removed from the MMR.
		// Rewound input (spent) pos will be added back to the MMR.
		let head_header = self.batch.get_block_header(&self.head.hash())?;
		let rewind_rm_pos = input_pos_to_rewind(header, &head_header, &self.batch)?;

		self.rewind_to_pos(
			header.output_i_mmr_size,
			header.output_ii_mmr_size,
			header.kernel_mmr_size,
			&rewind_rm_pos,
		)?;

		// Update our head to reflect the header we rewound to.
		self.head = Tip::from_header(header);

		Ok(())
	}

	/// Rewinds the MMRs to the provided positions, given the output and
	/// kernel we want to rewind to.
	fn rewind_to_pos(
		&mut self,
		output_i_pos: u64,
		output_ii_pos: u64,
		kernel_pos: u64,
		rewind_rm_pos: &Bitmap,
	) -> Result<(), Error> {
		self.output_i_pmmr
			.rewind(output_i_pos, rewind_rm_pos)
			.map_err(&ErrorKind::TxHashSetErr)?;
		self.output_ii_pmmr
			.rewind(output_ii_pos, rewind_rm_pos)
			.map_err(&ErrorKind::TxHashSetErr)?;
		self.kernel_pmmr
			.rewind(kernel_pos, &Bitmap::create())
			.map_err(&ErrorKind::TxHashSetErr)?;
		Ok(())
	}

	/// Current root hashes and sums (if applicable) for the Output, range proof
	/// and kernel sum trees.
	pub fn roots(&self) -> Result<TxHashSetRoots, Error> {
		Ok(TxHashSetRoots {
			output_i_root: self
				.output_i_pmmr
				.root()
				.map_err(|_| ErrorKind::InvalidRoot)?,
			output_ii_root: self
				.output_ii_pmmr
				.root()
				.map_err(|_| ErrorKind::InvalidRoot)?,
			kernel_root: self
				.kernel_pmmr
				.root()
				.map_err(|_| ErrorKind::InvalidRoot)?,
		})
	}

	/// Validate the MMR (output_i, output_ii, kernel) roots against the latest header.
	pub fn validate_roots(&self) -> Result<(), Error> {
		if self.head.height == 0 {
			return Ok(());
		}
		let head_header = self.batch.get_block_header(&self.head.hash())?;
		let header_roots = TxHashSetRoots {
			output_i_root: head_header.output_i_root,
			output_ii_root: head_header.output_ii_root,
			kernel_root: head_header.kernel_root,
		};
		if header_roots != self.roots()? {
			Err(ErrorKind::InvalidRoot.into())
		} else {
			Ok(())
		}
	}

	/// Validate the header, output and kernel MMR sizes against the block header.
	pub fn validate_sizes(&self) -> Result<(), Error> {
		if self.head.height == 0 {
			return Ok(());
		}
		let head_header = self.batch.get_block_header(&self.head.last_block_h)?;
		if (
			head_header.output_i_mmr_size,
			head_header.output_ii_mmr_size,
			head_header.kernel_mmr_size,
		) != self.sizes()
		{
			Err(ErrorKind::InvalidMMRSize.into())
		} else {
			Ok(())
		}
	}

	fn validate_mmrs(&self) -> Result<(), Error> {
		let now = Instant::now();

		// validate all hashes and sums within the trees
		if let Err(e) = self.output_i_pmmr.validate() {
			return Err(ErrorKind::InvalidTxHashSet(e).into());
		}
		if let Err(e) = self.output_ii_pmmr.validate() {
			return Err(ErrorKind::InvalidTxHashSet(e).into());
		}
		if let Err(e) = self.kernel_pmmr.validate() {
			return Err(ErrorKind::InvalidTxHashSet(e).into());
		}

		debug!(
			"txhashset: validated the outputI {}, outputII {}, kernel {} mmrs, took {}s",
			self.output_i_pmmr.unpruned_size(),
			self.output_ii_pmmr.unpruned_size(),
			self.kernel_pmmr.unpruned_size(),
			now.elapsed().as_secs(),
		);

		Ok(())
	}

	/// Validate full kernel sums against the provided header (for overage and kernel_offset).
	/// This is an expensive operation as we need to retrieve all the UTXOs and kernels
	/// from the respective MMRs.
	/// For a significantly faster way of validating full kernel sums see BlockSums.
	pub fn validate_kernel_sums(
		&self,
		genesis: &BlockHeader,
	) -> Result<((Commitment, Commitment)), Error> {
		let now = Instant::now();

		let head_header = self.batch.get_block_header(&self.head.last_block_h)?;
		let (utxo_sum, kernel_sum) = self.verify_kernel_sums()?;
		//todo: total overage validation
		let _overage = head_header.total_overage(genesis.kernel_mmr_size > 0);

		debug!(
			"txhashset: validated total kernel sums, took {}s",
			now.elapsed().as_secs(),
		);

		Ok((utxo_sum, kernel_sum))
	}

	/// Validate the txhashset state against the provided block header.
	/// A "fast validation" will skip rangeproof verification and kernel signature verification.
	pub fn validate(
		&self,
		genesis: &BlockHeader,
		fast_validation: bool,
		status: &dyn TxHashsetWriteStatus,
	) -> Result<((Commitment, Commitment)), Error> {
		self.validate_mmrs()?;
		self.validate_roots()?;
		self.validate_sizes()?;

		if self.head.height == 0 {
			let zero_commit = secp_static::commit_to_zero_value();
			return Ok((zero_commit.clone(), zero_commit.clone()));
		}

		// The real magicking happens here. Sum of kernel excesses should equal
		// sum of unspent outputs minus total supply.
		let (output_sum, kernel_sum) = self.validate_kernel_sums(genesis)?;

		// These are expensive verification step (skipped for "fast validation").
		if !fast_validation {
			// Verify all the kernel signatures.
			self.verify_kernel_signatures(status)?;
		}

		Ok((output_sum, kernel_sum))
	}

	/// Force the rollback of this extension, no matter the result
	pub fn force_rollback(&mut self) {
		self.rollback = true;
	}

	/// Dumps the output MMR.
	/// We use this after compacting for visual confirmation that it worked.
	pub fn dump_output_pmmr(&self) {
		debug!("-- outputs --");
		self.output_i_pmmr.dump_from_file(false);
		self.output_ii_pmmr.dump_from_file(false);
		debug!("--");
		self.output_i_pmmr.dump_stats();
		self.output_ii_pmmr.dump_stats();
		debug!("-- end of outputs --");
	}

	/// Dumps the state of the 3 sum trees to stdout for debugging. Short
	/// version only prints the Output tree.
	pub fn dump(&self, short: bool) {
		debug!("-- outputs --");
		self.output_i_pmmr.dump(short);
		self.output_ii_pmmr.dump(short);
		if !short {
			debug!("-- kernels --");
			self.kernel_pmmr.dump(short);
		}
	}

	/// Sizes of each of the sum trees
	pub fn sizes(&self) -> (u64, u64, u64) {
		(
			self.output_i_pmmr.unpruned_size(),
			self.output_ii_pmmr.unpruned_size(),
			self.kernel_pmmr.unpruned_size(),
		)
	}

	fn verify_kernel_signatures(&self, status: &dyn TxHashsetWriteStatus) -> Result<(), Error> {
		let now = Instant::now();
		const KERNEL_BATCH_SIZE: usize = 5_000;

		let mut kern_count = 0;
		let total_kernels = pmmr::n_leaves(self.kernel_pmmr.unpruned_size());
		let mut tx_kernels: Vec<TxKernel> = Vec::with_capacity(KERNEL_BATCH_SIZE);
		for n in 1..self.kernel_pmmr.unpruned_size() + 1 {
			if pmmr::is_leaf(n) {
				let kernel = self
					.kernel_pmmr
					.get_data(n)
					.ok_or::<Error>(ErrorKind::TxKernelNotFound.into())?;

				tx_kernels.push(kernel);
			}

			if tx_kernels.len() >= KERNEL_BATCH_SIZE || n >= self.kernel_pmmr.unpruned_size() {
				TxKernel::batch_sig_verify(&tx_kernels)?;
				kern_count += tx_kernels.len() as u64;
				tx_kernels.clear();
				status.on_validation(kern_count, total_kernels, 0, 0);
				debug!(
					"txhashset: verify_kernel_signatures: verified {} signatures",
					kern_count,
				);
			}
		}

		debug!(
			"txhashset: verified {} kernel signatures, pmmr size {}, took {}s",
			kern_count,
			self.kernel_pmmr.unpruned_size(),
			now.elapsed().as_secs(),
		);

		Ok(())
	}
}

/// Packages the txhashset data files into a zip and returns a Read to the
/// resulting file
pub fn zip_read(root_dir: String, header: &BlockHeader) -> Result<File, Error> {
	let txhashset_zip = format!("{}_{}.zip", TXHASHSET_ZIP, header.hash().to_string());

	let txhashset_path = Path::new(&root_dir).join(TXHASHSET_SUBDIR);
	let zip_path = Path::new(&root_dir).join(txhashset_zip);

	// if file exist, just re-use it
	let zip_file = File::open(zip_path.clone());
	if let Ok(zip) = zip_file {
		return Ok(zip);
	} else {
		// clean up old zips.
		// Theoretically, we only need clean-up those zip files older than STATE_SYNC_THRESHOLD.
		// But practically, these zip files are not small ones, we just keep the zips in last 24 hours
		let data_dir = Path::new(&root_dir);
		let pattern = format!("{}_", TXHASHSET_ZIP);
		if let Ok(n) = clean_files_by_prefix(data_dir.clone(), &pattern, 24 * 60 * 60) {
			debug!(
				"{} zip files have been clean up in folder: {:?}",
				n, data_dir
			);
		}
	}

	// otherwise, create the zip archive
	let path_to_be_cleanup = {
		// Temp txhashset directory
		let temp_txhashset_path = Path::new(&root_dir).join(format!(
			"{}_zip_{}",
			TXHASHSET_SUBDIR,
			header.hash().to_string()
		));
		// Remove temp dir if it exist
		if temp_txhashset_path.exists() {
			fs::remove_dir_all(&temp_txhashset_path)?;
		}
		// Copy file to another dir
		file::copy_dir_to(&txhashset_path, &temp_txhashset_path)?;

		let zip_file = File::create(zip_path.clone())?;

		// Explicit list of files to add to our zip archive.
		let files = file_list(header);

		zip::create_zip(&zip_file, &temp_txhashset_path, files)?;

		temp_txhashset_path
	};

	// open it again to read it back
	let zip_file = File::open(zip_path.clone())?;

	// clean-up temp txhashset directory.
	if let Err(e) = fs::remove_dir_all(&path_to_be_cleanup) {
		warn!(
			"txhashset zip file: {:?} fail to remove, err: {}",
			zip_path.to_str(),
			e
		);
	}
	Ok(zip_file)
}

// Explicit list of files to extract from our zip archive.
// We include *only* these files when building the txhashset zip.
// We extract *only* these files when receiving a txhashset zip.
// Everything else will be safely ignored.
// Return Vec<PathBuf> as some of these are dynamic (specifically the "rewound" leaf files).
fn file_list(header: &BlockHeader) -> Vec<PathBuf> {
	vec![
		// kernel MMR
		PathBuf::from("kernel/pmmr_data.bin"),
		PathBuf::from("kernel/pmmr_hash.bin"),
		// OutputI MMR
		PathBuf::from("outputI/pmmr_data.bin"),
		PathBuf::from("outputI/pmmr_hash.bin"),
		PathBuf::from("outputI/pmmr_prun.bin"),
		// OutputII MMR
		PathBuf::from("outputII/pmmr_data.bin"),
		PathBuf::from("outputII/pmmr_hash.bin"),
		PathBuf::from("outputII/pmmr_prun.bin"),
		// Header specific "rewound" leaf files for output MMR.
		PathBuf::from(format!("outputI/pmmr_leaf.bin.{}", header.hash())),
		PathBuf::from(format!("outputII/pmmr_leaf.bin.{}", header.hash())),
	]
}

/// Extract the txhashset data from a zip file and writes the content into the
/// txhashset storage dir
pub fn zip_write(
	root_dir: PathBuf,
	txhashset_data: File,
	header: &BlockHeader,
) -> Result<(), Error> {
	debug!("zip_write on path: {:?}", root_dir);
	let txhashset_path = root_dir.clone().join(TXHASHSET_SUBDIR);
	fs::create_dir_all(&txhashset_path)?;

	// Explicit list of files to extract from our zip archive.
	let files = file_list(header);

	// We expect to see *exactly* the paths listed above.
	// No attempt is made to be permissive or forgiving with "alternative" paths.
	// These are the *only* files we will attempt to extract from the zip file.
	// If any of these are missing we will attempt to continue as some are potentially optional.
	zip::extract_files(txhashset_data, &txhashset_path, files)?;
	Ok(())
}

/// Overwrite txhashset folders in "to" folder with "from" folder
pub fn txhashset_replace(from: PathBuf, to: PathBuf) -> Result<(), Error> {
	debug!("txhashset_replace: move from {:?} to {:?}", from, to);

	// clean the 'to' folder firstly
	clean_txhashset_folder(&to);

	// rename the 'from' folder as the 'to' folder
	if let Err(e) = fs::rename(
		from.clone().join(TXHASHSET_SUBDIR),
		to.clone().join(TXHASHSET_SUBDIR),
	) {
		error!("hashset_replace fail on {}. err: {}", TXHASHSET_SUBDIR, e);
		Err(ErrorKind::TxHashSetErr(format!("txhashset replacing fail")).into())
	} else {
		Ok(())
	}
}

/// Clean the txhashset folder
pub fn clean_txhashset_folder(root_dir: &PathBuf) {
	let txhashset_path = root_dir.clone().join(TXHASHSET_SUBDIR);
	if txhashset_path.exists() {
		if let Err(e) = fs::remove_dir_all(txhashset_path.clone()) {
			warn!(
				"clean_txhashset_folder: fail on {:?}. err: {}",
				txhashset_path, e
			);
		}
	}
}

/// Given a block header to rewind to and the block header at the
/// head of the current chain state, we need to calculate the positions
/// of all inputs (spent outputs) we need to "undo" during a rewind.
/// We do this by leveraging the "block_input_bitmap" cache and OR'ing
/// the set of bitmaps together for the set of blocks being rewound.
fn input_pos_to_rewind(
	block_header: &BlockHeader,
	head_header: &BlockHeader,
	batch: &Batch<'_>,
) -> Result<Bitmap, Error> {
	if head_header.height <= block_header.height {
		return Ok(Bitmap::create());
	}

	// Batching up the block input bitmaps, and running fast_or() on every batch of 256 bitmaps.
	// so to avoid maintaining a huge vec of bitmaps.
	let bitmap_fast_or = |b_res, block_input_bitmaps: &mut Vec<Bitmap>| -> Option<Bitmap> {
		if let Some(b) = b_res {
			block_input_bitmaps.push(b);
			if block_input_bitmaps.len() < 256 {
				return None;
			}
		}
		let bitmap = Bitmap::fast_or(&block_input_bitmaps.iter().collect::<Vec<&Bitmap>>());
		block_input_bitmaps.clear();
		block_input_bitmaps.push(bitmap.clone());
		Some(bitmap)
	};

	let mut block_input_bitmaps: Vec<Bitmap> = vec![];

	let mut current = head_header.clone();
	while current.hash() != block_header.hash() {
		if current.height < 1 {
			break;
		}

		// I/O should be minimized or eliminated here for most
		// rewind scenarios.
		if let Ok(b_res) = batch.get_block_input_bitmap(&current.hash()) {
			bitmap_fast_or(Some(b_res), &mut block_input_bitmaps);
		}
		current = batch.get_previous_header(&current)?;
	}

	bitmap_fast_or(None, &mut block_input_bitmaps).ok_or_else(|| ErrorKind::Bitmap.into())
}
