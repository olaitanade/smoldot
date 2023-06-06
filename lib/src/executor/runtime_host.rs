// Smoldot
// Copyright (C) 2019-2022  Parity Technologies (UK) Ltd.
// SPDX-License-Identifier: GPL-3.0-or-later WITH Classpath-exception-2.0

// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with this program.  If not, see <http://www.gnu.org/licenses/>.

//! Wasm virtual machine, with automatic storage overlay and logs management.
//!
//! The code in this module builds upon the functionalities of the [`host`] module and
//! implements some of the host function calls. In other words, it is an easier-to-use version of
//! the [`host`] module.
//!
//! Most of the documentation of the [`host`] module also applies here.
//!
//! In addition to the functionalities provided by the [`host`] module, the `runtime_host` module:
//!
//! - Keeps track of the changes to the storage and off-chain storage made by the execution, and
//!   provides them at the end. Any storage access takes into account the intermediary list of
//!   changes.
//! - Keeps track of the logs generated by the call and concatenates them into a [`String`].
//! - Automatically handles some externalities, such as calculating the Merkle root or storage
//!   transactions.
//!
//! These additional features considerably reduces the number of externals concepts to plug to
//! the virtual machine.

// TODO: more docs

use crate::{
    executor::{self, host, storage_diff, trie_root_calculator, vm},
    trie, util,
};

use alloc::{borrow::ToOwned as _, string::String, vec::Vec};
use core::{fmt, iter};

pub use trie::{Nibble, TrieEntryVersion};

/// Configuration for [`run`].
pub struct Config<'a, TParams> {
    /// Virtual machine to be run.
    pub virtual_machine: host::HostVmPrototype,

    /// Name of the function to be called.
    pub function_to_call: &'a str,

    /// Parameter of the call, as an iterator of bytes. The concatenation of bytes forms the
    /// actual input.
    pub parameter: TParams,

    /// Initial state of [`Success::storage_main_trie_changes`]. The changes made during this
    /// execution will be pushed over the value in this field.
    pub storage_main_trie_changes: storage_diff::TrieDiff,

    /// Initial state of [`Success::offchain_storage_changes`]. The changes made during this
    /// execution will be pushed over the value in this field.
    pub offchain_storage_changes: storage_diff::TrieDiff,

    /// Maximum log level of the runtime.
    ///
    /// > **Note**: This value is opaque from the point of the view of the client, and the runtime
    /// >           is free to interpret it the way it wants. However, usually values are: `0` for
    /// >           "off", `1` for "error", `2` for "warn", `3` for "info", `4` for "debug",
    /// >           and `5` for "trace".
    pub max_log_level: u32,
}

/// Start running the WebAssembly virtual machine.
pub fn run(
    config: Config<impl Iterator<Item = impl AsRef<[u8]>> + Clone>,
) -> Result<RuntimeHostVm, (host::StartErr, host::HostVmPrototype)> {
    let state_trie_version = config
        .virtual_machine
        .runtime_version()
        .decode()
        .state_version
        .unwrap_or(TrieEntryVersion::V0);

    Ok(Inner {
        vm: config
            .virtual_machine
            .run_vectored(config.function_to_call, config.parameter)?
            .into(),
        main_trie_changes: config.storage_main_trie_changes,
        state_trie_version,
        main_trie_transaction: Vec::new(),
        offchain_storage_changes: config.offchain_storage_changes,
        root_calculation: None,
        logs: String::new(),
        max_log_level: config.max_log_level,
    }
    .run())
}

/// Execution is successful.
#[derive(Debug)]
pub struct Success {
    /// Contains the output value of the runtime, and the virtual machine that was passed at
    /// initialization.
    pub virtual_machine: SuccessVirtualMachine,
    /// List of changes to the storage main trie that the block performs.
    pub storage_main_trie_changes: storage_diff::TrieDiff,
    /// State trie version indicated by the runtime. All the storage changes indicated by
    /// [`Success::storage_main_trie_changes`] should store this version alongside with them.
    pub state_trie_version: TrieEntryVersion,
    /// List of changes to the off-chain storage that this block performs.
    pub offchain_storage_changes: storage_diff::TrieDiff,
    /// Concatenation of all the log messages printed by the runtime.
    pub logs: String,
}

/// Function execution has succeeded. Contains the return value of the call.
pub struct SuccessVirtualMachine(host::Finished);

impl SuccessVirtualMachine {
    /// Returns the value the called function has returned.
    pub fn value(&'_ self) -> impl AsRef<[u8]> + '_ {
        self.0.value()
    }

    /// Turns the virtual machine back into a prototype.
    pub fn into_prototype(self) -> host::HostVmPrototype {
        self.0.into_prototype()
    }
}

impl fmt::Debug for SuccessVirtualMachine {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_tuple("SuccessVirtualMachine").finish()
    }
}

/// Error that can happen during the execution.
#[derive(Debug, derive_more::Display)]
#[display(fmt = "{detail}")]
pub struct Error {
    /// Exact error that happened.
    pub detail: ErrorDetail,
    /// Prototype of the virtual machine that was passed through [`Config::virtual_machine`].
    pub prototype: host::HostVmPrototype,
}

/// See [`Error::detail`].
#[derive(Debug, Clone, derive_more::Display)]
pub enum ErrorDetail {
    /// Error while executing the Wasm virtual machine.
    #[display(fmt = "Error while executing Wasm VM: {error}\n{logs:?}")]
    WasmVm {
        /// Error that happened.
        error: host::Error,
        /// Concatenation of all the log messages printed by the runtime.
        logs: String,
    },
    /// Size of the logs generated by the runtime exceeds the limit.
    LogsTooLong,
}

/// Current state of the execution.
#[must_use]
pub enum RuntimeHostVm {
    /// Execution is over.
    Finished(Result<Success, Error>),
    /// Loading a storage value is required in order to continue.
    StorageGet(StorageGet),
    /// Obtaining the Merkle value of the closest descendant of a trie node is required in order
    /// to continue.
    ClosestDescendantMerkleValue(ClosestDescendantMerkleValue),
    /// Fetching the key that follows a given one is required in order to continue.
    NextKey(NextKey),
    /// Verifying whether a signature is correct is required in order to continue.
    SignatureVerification(SignatureVerification),
}

impl RuntimeHostVm {
    /// Cancels execution of the virtual machine and returns back the prototype.
    pub fn into_prototype(self) -> host::HostVmPrototype {
        match self {
            RuntimeHostVm::Finished(Ok(inner)) => inner.virtual_machine.into_prototype(),
            RuntimeHostVm::Finished(Err(inner)) => inner.prototype,
            RuntimeHostVm::StorageGet(inner) => inner.inner.vm.into_prototype(),
            RuntimeHostVm::ClosestDescendantMerkleValue(inner) => inner.inner.vm.into_prototype(),
            RuntimeHostVm::NextKey(inner) => inner.inner.vm.into_prototype(),
            RuntimeHostVm::SignatureVerification(inner) => inner.inner.vm.into_prototype(),
        }
    }
}

/// Loading a storage value is required in order to continue.
#[must_use]
pub struct StorageGet {
    inner: Inner,
}

impl StorageGet {
    /// Returns the key whose value must be passed to [`StorageGet::inject_value`].
    pub fn key(&'_ self) -> impl AsRef<[u8]> + '_ {
        enum Three<A, B, C> {
            A(A),
            B(B),
            C(C),
        }

        impl<A: AsRef<[u8]>, B: AsRef<[u8]>, C: AsRef<[u8]>> AsRef<[u8]> for Three<A, B, C> {
            fn as_ref(&self) -> &[u8] {
                match self {
                    Three::A(a) => a.as_ref(),
                    Three::B(b) => b.as_ref(),
                    Three::C(c) => c.as_ref(),
                }
            }
        }

        match &self.inner.vm {
            host::HostVm::ExternalStorageGet(req) => Three::A(req.key()),
            host::HostVm::ExternalStorageAppend(req) => Three::B(req.key()),
            host::HostVm::ExternalStorageRoot(_) => {
                if let trie_root_calculator::InProgress::StorageValue(value_request) =
                    self.inner.root_calculation.as_ref().unwrap()
                {
                    // TODO: optimize?
                    let key_nibbles = value_request.key().fold(Vec::new(), |mut a, b| {
                        a.extend_from_slice(b.as_ref());
                        a
                    });
                    debug_assert_eq!(key_nibbles.len() % 2, 0);
                    Three::C(
                        trie::nibbles_to_bytes_suffix_extend(key_nibbles.into_iter())
                            .collect::<Vec<_>>(),
                    )
                } else {
                    // We only create a `StorageGet` if the state is `StorageValue`.
                    panic!()
                }
            }

            // We only create a `StorageGet` if the state is one of the above.
            _ => unreachable!(),
        }
    }

    /// Injects the corresponding storage value.
    pub fn inject_value(
        mut self,
        value: Option<(impl Iterator<Item = impl AsRef<[u8]>>, TrieEntryVersion)>,
    ) -> RuntimeHostVm {
        // TODO: update the implementation to not require the folding here
        let value = value.map(|(value, version)| {
            let value = value.fold(Vec::new(), |mut a, b| {
                a.extend_from_slice(b.as_ref());
                a
            });
            (value, version)
        });

        match self.inner.vm {
            host::HostVm::ExternalStorageGet(req) => {
                // TODO: should actually report the offset and max_size in the API
                self.inner.vm = req.resume_full_value(value.as_ref().map(|(v, _)| &v[..]));
            }
            host::HostVm::ExternalStorageAppend(req) => {
                // TODO: could be less overhead?
                let mut value = value.map(|(v, _)| v).unwrap_or_default();
                append_to_storage_value(&mut value, req.value().as_ref());
                self.inner
                    .main_trie_changes
                    .diff_insert(req.key().as_ref().to_vec(), value, ());

                self.inner.vm = req.resume();
            }
            host::HostVm::ExternalStorageRoot(_) => {
                if let trie_root_calculator::InProgress::StorageValue(value_request) =
                    self.inner.root_calculation.take().unwrap()
                {
                    self.inner.root_calculation = Some(
                        value_request.inject_value(value.as_ref().map(|(v, vers)| (&v[..], *vers))),
                    );
                } else {
                    // We only create a `StorageGet` if the state is `StorageValue`.
                    panic!()
                }
            }

            // We only create a `StorageGet` if the state is one of the above.
            _ => unreachable!(),
        };

        self.inner.run()
    }
}

/// Fetching the key that follows a given one is required in order to continue.
#[must_use]
pub struct NextKey {
    inner: Inner,

    /// If `Some`, ask for the key inside of this field rather than the one of `inner`.
    key_overwrite: Option<Vec<u8>>,

    /// Number of keys removed. Used only to implement clearing a prefix, otherwise stays at 0.
    keys_removed_so_far: u32,
}

impl NextKey {
    /// Returns the key whose next key must be passed back.
    pub fn key(&'_ self) -> impl Iterator<Item = Nibble> + '_ {
        if let Some(key_overwrite) = &self.key_overwrite {
            return either::Left(trie::bytes_to_nibbles(key_overwrite.iter().copied()));
        }

        either::Right(match &self.inner.vm {
            host::HostVm::ExternalStorageNextKey(req) => {
                either::Left(trie::bytes_to_nibbles(util::as_ref_iter(req.key())))
            }

            host::HostVm::ExternalStorageRoot(_) => {
                let Some(trie_root_calculator::InProgress::ClosestDescendant(req)) = &self.inner.root_calculation
                    else { unreachable!() };
                either::Right(req.key().flat_map(util::as_ref_iter))
            }

            // Note that in the case `ExternalStorageClearPrefix`, `key_overwrite` is
            // always `Some`.
            _ => unreachable!(),
        })
    }

    /// If `true`, then the provided value must the one superior or equal to the requested key.
    /// If `false`, then the provided value must be strictly superior to the requested key.
    pub fn or_equal(&self) -> bool {
        (matches!(self.inner.vm, host::HostVm::ExternalStorageClearPrefix(_))
            && self.keys_removed_so_far == 0)
            || matches!(self.inner.vm, host::HostVm::ExternalStorageRoot(_))
    }

    /// If `true`, then the search must include both branch nodes and storage nodes. If `false`,
    /// the search only covers storage nodes.
    pub fn branch_nodes(&self) -> bool {
        matches!(self.inner.vm, host::HostVm::ExternalStorageRoot(_))
    }

    /// Returns the prefix the next key must start with. If the next key doesn't start with the
    /// given prefix, then `None` should be provided.
    pub fn prefix(&'_ self) -> impl Iterator<Item = Nibble> + '_ {
        match &self.inner.vm {
            host::HostVm::ExternalStorageClearPrefix(req) => {
                either::Left(trie::bytes_to_nibbles(util::as_ref_iter(req.prefix())))
            }
            host::HostVm::ExternalStorageRoot(_) => either::Right(either::Left(self.key())),
            _ => either::Right(either::Right(iter::empty())),
        }
    }

    /// Injects the key.
    ///
    /// # Panic
    ///
    /// Panics if the key passed as parameter isn't strictly superior to the requested key.
    /// Panics if the key passed as parameter doesn't start with the requested prefix.
    ///
    pub fn inject_key(mut self, key: Option<impl Iterator<Item = Nibble>>) -> RuntimeHostVm {
        match self.inner.vm {
            host::HostVm::ExternalStorageNextKey(req) => {
                let key =
                    key.map(|key| trie::nibbles_to_bytes_suffix_extend(key).collect::<Vec<_>>());

                let search = {
                    let req_key = req.key();
                    let requested_key = if let Some(key_overwrite) = &self.key_overwrite {
                        &key_overwrite[..]
                    } else {
                        req_key.as_ref()
                    };
                    self.inner.main_trie_changes.storage_next_key(
                        requested_key,
                        key.as_deref(),
                        false,
                    )
                };

                match search {
                    storage_diff::StorageNextKey::Found(k) => {
                        self.inner.vm = req.resume(k);
                    }
                    storage_diff::StorageNextKey::NextOf(next) => {
                        let key_overwrite = Some(next.to_owned());
                        self.inner.vm = host::HostVm::ExternalStorageNextKey(req);
                        return RuntimeHostVm::NextKey(NextKey {
                            inner: self.inner,
                            key_overwrite,
                            keys_removed_so_far: 0,
                        });
                    }
                }
            }

            host::HostVm::ExternalStorageClearPrefix(req) => {
                // TODO: there's some trickiness regarding the behavior w.r.t keys only in the overlay; figure out

                if let Some(key) = key {
                    let key = trie::nibbles_to_bytes_suffix_extend(key).collect::<Vec<_>>();
                    assert!(key.starts_with(req.prefix().as_ref()));

                    // TODO: /!\ must clear keys from overlay as well

                    if req
                        .max_keys_to_remove()
                        .map_or(false, |max| self.keys_removed_so_far >= max)
                    {
                        self.inner.vm = req.resume(self.keys_removed_so_far, true);
                    } else {
                        self.inner
                            .main_trie_changes
                            .diff_insert_erase(key.clone(), ());
                        self.keys_removed_so_far += 1;
                        self.key_overwrite = Some(key); // TODO: might be expensive if lots of keys
                        self.inner.vm = req.into();

                        return RuntimeHostVm::NextKey(self);
                    }
                } else {
                    self.inner.vm = req.resume(self.keys_removed_so_far, false);
                }
            }

            host::HostVm::ExternalStorageRoot(_) => {
                let Some(trie_root_calculator::InProgress::ClosestDescendant(req)) = self.inner.root_calculation.take()
                    else { unreachable!() };
                self.inner.root_calculation = Some(req.inject(key));
            }

            // We only create a `NextKey` if the state is one of the above.
            _ => unreachable!(),
        };

        self.inner.run()
    }
}

/// Obtaining the Merkle value of the closest descendant of a trie node is required in order to
/// continue.
#[must_use]
pub struct ClosestDescendantMerkleValue {
    inner: Inner,
}

impl ClosestDescendantMerkleValue {
    /// Returns the key whose closest descendant Merkle value must be passed to
    /// [`ClosestDescendantMerkleValue::inject_merkle_value`].
    pub fn key(&'_ self) -> impl Iterator<Item = Nibble> + '_ {
        debug_assert!(matches!(
            &self.inner.vm,
            host::HostVm::ExternalStorageRoot(_)
        ));

        let trie_root_calculator::InProgress::ClosestDescendantMerkleValue(request) =
            self.inner.root_calculation.as_ref().unwrap()
            else { unreachable!() };
        request.key().flat_map(util::as_ref_iter)
    }

    /// Indicate that the value is unknown and resume the calculation.
    ///
    /// This function be used if you are unaware of the Merkle value. The algorithm will perform
    /// the calculation of this Merkle value manually, which takes more time.
    pub fn resume_unknown(mut self) -> RuntimeHostVm {
        debug_assert!(matches!(
            &self.inner.vm,
            host::HostVm::ExternalStorageRoot(_)
        ));

        let trie_root_calculator::InProgress::ClosestDescendantMerkleValue(request) =
            self.inner.root_calculation.take().unwrap()
            else { unreachable!() };

        self.inner.root_calculation = Some(request.resume_unknown());
        self.inner.run()
    }

    /// Injects the corresponding Merkle value.
    pub fn inject_merkle_value(mut self, merkle_value: &[u8]) -> RuntimeHostVm {
        debug_assert!(matches!(
            &self.inner.vm,
            host::HostVm::ExternalStorageRoot(_)
        ));

        let trie_root_calculator::InProgress::ClosestDescendantMerkleValue(request) =
            self.inner.root_calculation.take().unwrap()
            else { unreachable!() };

        self.inner.root_calculation = Some(request.inject_merkle_value(merkle_value));
        self.inner.run()
    }
}

/// Verifying whether a signature is correct is required in order to continue.
#[must_use]
pub struct SignatureVerification {
    inner: Inner,
}

impl SignatureVerification {
    /// Returns the message that the signature is expected to sign.
    pub fn message(&'_ self) -> impl AsRef<[u8]> + '_ {
        match self.inner.vm {
            host::HostVm::SignatureVerification(ref sig) => sig.message(),
            _ => unreachable!(),
        }
    }

    /// Returns the signature.
    ///
    /// > **Note**: Be aware that this signature is untrusted input and might not be part of the
    /// >           set of valid signatures.
    pub fn signature(&'_ self) -> impl AsRef<[u8]> + '_ {
        match self.inner.vm {
            host::HostVm::SignatureVerification(ref sig) => sig.signature(),
            _ => unreachable!(),
        }
    }

    /// Returns the public key the signature is against.
    ///
    /// > **Note**: Be aware that this public key is untrusted input and might not be part of the
    /// >           set of valid public keys.
    pub fn public_key(&'_ self) -> impl AsRef<[u8]> + '_ {
        match self.inner.vm {
            host::HostVm::SignatureVerification(ref sig) => sig.public_key(),
            _ => unreachable!(),
        }
    }

    /// Verify the signature. Returns `true` if it is valid.
    pub fn is_valid(&self) -> bool {
        match self.inner.vm {
            host::HostVm::SignatureVerification(ref sig) => sig.is_valid(),
            _ => unreachable!(),
        }
    }

    /// Verify the signature and resume execution.
    pub fn verify_and_resume(mut self) -> RuntimeHostVm {
        match self.inner.vm {
            host::HostVm::SignatureVerification(sig) => self.inner.vm = sig.verify_and_resume(),
            _ => unreachable!(),
        }

        self.inner.run()
    }

    /// Resume the execution assuming that the signature is valid.
    ///
    /// > **Note**: You are strongly encouraged to call
    /// >           [`SignatureVerification::verify_and_resume`]. This function is meant to be
    /// >           used only in debugging situations.
    pub fn resume_success(mut self) -> RuntimeHostVm {
        match self.inner.vm {
            host::HostVm::SignatureVerification(sig) => self.inner.vm = sig.resume_success(),
            _ => unreachable!(),
        }

        self.inner.run()
    }

    /// Resume the execution assuming that the signature is invalid.
    ///
    /// > **Note**: You are strongly encouraged to call
    /// >           [`SignatureVerification::verify_and_resume`]. This function is meant to be
    /// >           used only in debugging situations.
    pub fn resume_failed(mut self) -> RuntimeHostVm {
        match self.inner.vm {
            host::HostVm::SignatureVerification(sig) => self.inner.vm = sig.resume_failed(),
            _ => unreachable!(),
        }

        self.inner.run()
    }
}

/// Implementation detail of the execution. Shared by all the variants of [`RuntimeHostVm`]
/// other than [`RuntimeHostVm::Finished`].
struct Inner {
    /// Virtual machine running the call.
    vm: host::HostVm,

    /// Pending changes to the top storage trie that this execution performs.
    main_trie_changes: storage_diff::TrieDiff,

    /// Contains a copy of [`Inner::main_trie_changes`] at the time when the transaction started.
    /// When the storage transaction ends, either the entry is silently discarded (to commit),
    /// or is written over [`Inner::main_trie_changes`] (to rollback).
    ///
    /// Contains a `Vec` in case transactions are stacked.
    main_trie_transaction: Vec<storage_diff::TrieDiff>,

    /// State trie version indicated by the runtime. All the storage changes that are performed
    /// use this version.
    state_trie_version: TrieEntryVersion,

    /// Pending changes to the off-chain storage that this execution performs.
    offchain_storage_changes: storage_diff::TrieDiff,

    /// Trie root calculation in progress.
    root_calculation: Option<trie_root_calculator::InProgress>,

    /// Concatenation of all the log messages generated by the runtime.
    logs: String,

    /// Value provided by [`Config::max_log_level`].
    max_log_level: u32,
}

impl Inner {
    /// Continues the execution.
    fn run(mut self) -> RuntimeHostVm {
        loop {
            match self.vm {
                host::HostVm::ReadyToRun(r) => self.vm = r.run(),

                host::HostVm::Error { error, prototype } => {
                    return RuntimeHostVm::Finished(Err(Error {
                        detail: ErrorDetail::WasmVm {
                            error,
                            logs: self.logs,
                        },
                        prototype,
                    }));
                }

                host::HostVm::Finished(finished) => {
                    return RuntimeHostVm::Finished(Ok(Success {
                        virtual_machine: SuccessVirtualMachine(finished),
                        storage_main_trie_changes: self.main_trie_changes,
                        state_trie_version: self.state_trie_version,
                        offchain_storage_changes: self.offchain_storage_changes,
                        logs: self.logs,
                    }));
                }

                host::HostVm::ExternalStorageGet(req) => {
                    if !matches!(req.trie(), host::Trie::MainTrie) {
                        // TODO: this is a dummy implementation and child tries are not implemented properly
                        self.vm = req.resume(None);
                        continue;
                    }

                    let search = self.main_trie_changes.diff_get(req.key().as_ref());
                    if let Some((overlay, _)) = search {
                        self.vm = req.resume_full_value(overlay);
                    } else {
                        self.vm = req.into();
                        return RuntimeHostVm::StorageGet(StorageGet { inner: self });
                    }
                }

                host::HostVm::ExternalStorageSet(req) => {
                    if !matches!(req.trie(), host::Trie::MainTrie) {
                        // TODO: this is a dummy implementation and child tries are not implemented properly
                        self.vm = req.resume();
                        continue;
                    }

                    if let Some(value) = req.value() {
                        self.main_trie_changes
                            .diff_insert(req.key().as_ref(), value.as_ref(), ());
                    } else {
                        self.main_trie_changes
                            .diff_insert_erase(req.key().as_ref(), ());
                    }

                    self.vm = req.resume()
                }

                host::HostVm::ExternalStorageAppend(req) => {
                    if !matches!(req.trie(), host::Trie::MainTrie) {
                        // TODO: this is a dummy implementation and child tries are not implemented properly
                        self.vm = req.resume();
                        continue;
                    }

                    let current_value = self
                        .main_trie_changes
                        .diff_get(req.key().as_ref())
                        .map(|(v, _)| v);
                    if let Some(current_value) = current_value {
                        let mut current_value = current_value.unwrap_or_default().to_vec();
                        append_to_storage_value(&mut current_value, req.value().as_ref());
                        self.main_trie_changes.diff_insert(
                            req.key().as_ref().to_vec(),
                            current_value,
                            (),
                        );
                        self.vm = req.resume();
                    } else {
                        self.vm = req.into();
                        return RuntimeHostVm::StorageGet(StorageGet { inner: self });
                    }
                }

                host::HostVm::ExternalStorageClearPrefix(req) => {
                    // TODO: this is a dummy implementation and child tries are not implemented properly
                    if !matches!(req.trie(), host::Trie::MainTrie) {
                        self.vm = req.resume(0, false);
                        continue;
                    }

                    let prefix = req.prefix().as_ref().to_owned();

                    self.vm = req.into();
                    return RuntimeHostVm::NextKey(NextKey {
                        inner: self,
                        key_overwrite: Some(prefix),
                        keys_removed_so_far: 0,
                    });
                }

                host::HostVm::ExternalStorageRoot(req) => {
                    let is_main_trie = matches!(req.trie(), host::Trie::MainTrie);
                    if !is_main_trie {
                        // TODO: this is a dummy implementation and child tries are not implemented properly
                        self.vm = req.resume(None);
                        continue;
                    }

                    if self.root_calculation.is_none() {
                        self.root_calculation = Some(trie_root_calculator::trie_root_calculator(
                            trie_root_calculator::Config {
                                diff: self.main_trie_changes.clone(), // TODO: don't clone?
                                diff_trie_entries_version: self.state_trie_version,
                                max_trie_recalculation_depth_hint: 16, // TODO: ?!
                            },
                        ));
                    }

                    match self.root_calculation.take().unwrap() {
                        trie_root_calculator::InProgress::ClosestDescendant(calc_req) => {
                            self.vm = req.into();
                            self.root_calculation = Some(
                                trie_root_calculator::InProgress::ClosestDescendant(calc_req),
                            );
                            return RuntimeHostVm::NextKey(NextKey {
                                inner: self,
                                key_overwrite: None,
                                keys_removed_so_far: 0,
                            });
                        }
                        trie_root_calculator::InProgress::StorageValue(calc_req) => {
                            self.vm = req.into();

                            if calc_req
                                .key()
                                .fold(0, |count, slice| count + slice.as_ref().len())
                                % 2
                                == 0
                            {
                                self.root_calculation =
                                    Some(trie_root_calculator::InProgress::StorageValue(calc_req));
                                return RuntimeHostVm::StorageGet(StorageGet { inner: self });
                            } else {
                                // If the number of nibbles in the key is uneven, we are sure that
                                // there exists no storage value.
                                self.root_calculation = Some(calc_req.inject_value(None));
                            }
                        }
                        trie_root_calculator::InProgress::ClosestDescendantMerkleValue(
                            calc_req,
                        ) => {
                            self.vm = req.into();
                            self.root_calculation = Some(
                                trie_root_calculator::InProgress::ClosestDescendantMerkleValue(
                                    calc_req,
                                ),
                            );
                            return RuntimeHostVm::ClosestDescendantMerkleValue(
                                ClosestDescendantMerkleValue { inner: self },
                            );
                        }
                        trie_root_calculator::InProgress::Finished { trie_root_hash } => {
                            self.vm = req.resume(Some(&trie_root_hash));
                        }
                    }
                }

                host::HostVm::ExternalStorageNextKey(req) => {
                    if matches!(req.trie(), host::Trie::MainTrie) {
                        self.vm = req.into();
                        return RuntimeHostVm::NextKey(NextKey {
                            inner: self,
                            key_overwrite: None,
                            keys_removed_so_far: 0,
                        });
                    } else {
                        // TODO: this is a dummy implementation and child tries are not implemented properly
                        self.vm = req.resume(None);
                    }
                }

                host::HostVm::ExternalOffchainStorageSet(req) => {
                    if let Some(value) = req.value() {
                        self.offchain_storage_changes.diff_insert(
                            req.key().as_ref().to_vec(),
                            value.as_ref().to_vec(),
                            (),
                        );
                    } else {
                        self.offchain_storage_changes
                            .diff_insert_erase(req.key().as_ref().to_vec(), ());
                    }

                    self.vm = req.resume();
                }

                host::HostVm::SignatureVerification(req) => {
                    self.vm = req.into();
                    return RuntimeHostVm::SignatureVerification(SignatureVerification {
                        inner: self,
                    });
                }

                host::HostVm::CallRuntimeVersion(req) => {
                    // TODO: make the user execute this ; see https://github.com/paritytech/smoldot/issues/144
                    // The code below compiles the provided WebAssembly runtime code, which is a
                    // relatively expensive operation (in the order of milliseconds).
                    // While it could be tempting to use a system cache, this function is expected
                    // to be called only right before runtime upgrades. Considering that runtime
                    // upgrades are quite uncommon and that a caching system is rather non-trivial
                    // to set up, the approach of recompiling every single time is preferred here.
                    // TODO: number of heap pages?! we use the default here, but not sure whether that's correct or if we have to take the current heap pages
                    let vm_prototype = match host::HostVmPrototype::new(host::Config {
                        module: req.wasm_code(),
                        heap_pages: executor::DEFAULT_HEAP_PAGES,
                        exec_hint: vm::ExecHint::Oneshot,
                        allow_unresolved_imports: false, // TODO: what is a correct value here?
                    }) {
                        Ok(w) => w,
                        Err(_) => {
                            self.vm = req.resume(Err(()));
                            continue;
                        }
                    };

                    self.vm = req.resume(Ok(vm_prototype.runtime_version().as_ref()));
                }

                host::HostVm::StartStorageTransaction(tx) => {
                    // TODO: this cloning is very expensive, but providing a more optimized implementation is very complicated
                    self.main_trie_transaction
                        .push(self.main_trie_changes.clone());
                    self.vm = tx.resume();
                }

                host::HostVm::EndStorageTransaction { resume, rollback } => {
                    // The inner implementation guarantees that a storage transaction can only
                    // end if it has earlier been started.
                    debug_assert!(!self.main_trie_transaction.is_empty());
                    let rollback_diff = self.main_trie_transaction.pop().unwrap();

                    if rollback {
                        self.main_trie_changes = rollback_diff;
                    }

                    self.vm = resume.resume();
                }

                host::HostVm::GetMaxLogLevel(resume) => {
                    self.vm = resume.resume(self.max_log_level);
                }

                host::HostVm::LogEmit(req) => {
                    // We add a hardcoded limit to the logs generated by the runtime in order to
                    // make sure that there is no memory leak. In practice, the runtime should
                    // rarely log more than a few hundred bytes. This limit is hardcoded rather
                    // than configurable because it is not expected to be reachable unless
                    // something is very wrong.
                    struct WriterWithMax<'a>(&'a mut String);
                    impl<'a> fmt::Write for WriterWithMax<'a> {
                        fn write_str(&mut self, s: &str) -> fmt::Result {
                            if self.0.len().saturating_add(s.len()) >= 1024 * 1024 {
                                return Err(fmt::Error);
                            }
                            self.0.push_str(s);
                            Ok(())
                        }
                        fn write_char(&mut self, c: char) -> fmt::Result {
                            if self.0.len().saturating_add(1) >= 1024 * 1024 {
                                return Err(fmt::Error);
                            }
                            self.0.push(c);
                            Ok(())
                        }
                    }
                    match fmt::write(&mut WriterWithMax(&mut self.logs), format_args!("{req}")) {
                        Ok(()) => {}
                        Err(fmt::Error) => {
                            return RuntimeHostVm::Finished(Err(Error {
                                detail: ErrorDetail::LogsTooLong,
                                prototype: host::HostVm::LogEmit(req).into_prototype(),
                            }));
                        }
                    }
                    self.vm = req.resume();
                }
            }
        }
    }
}

/// Performs the action described by [`host::HostVm::ExternalStorageAppend`] on an
/// encoded storage value.
fn append_to_storage_value(value: &mut Vec<u8>, to_add: &[u8]) {
    let (curr_len, curr_len_encoded_size) =
        match util::nom_scale_compact_usize::<nom::error::Error<&[u8]>>(value) {
            Ok((rest, l)) => (l, value.len() - rest.len()),
            Err(_) => {
                value.clear();
                value.reserve(to_add.len() + 1);
                value.extend_from_slice(util::encode_scale_compact_usize(1).as_ref());
                value.extend_from_slice(to_add);
                return;
            }
        };

    // Note: we use `checked_add`, as it is possible that the storage entry erroneously starts
    // with `u64::max_value()`.
    let new_len = match curr_len.checked_add(1) {
        Some(l) => l,
        None => {
            value.clear();
            value.reserve(to_add.len() + 1);
            value.extend_from_slice(util::encode_scale_compact_usize(1).as_ref());
            value.extend_from_slice(to_add);
            return;
        }
    };

    let new_len_encoded = util::encode_scale_compact_usize(new_len);

    let new_len_encoded_size = new_len_encoded.as_ref().len();
    debug_assert!(
        new_len_encoded_size == curr_len_encoded_size
            || new_len_encoded_size == curr_len_encoded_size + 1
    );

    for _ in 0..(new_len_encoded_size - curr_len_encoded_size) {
        value.insert(0, 0);
    }

    value[..new_len_encoded_size].copy_from_slice(new_len_encoded.as_ref());
    value.extend_from_slice(to_add);
}
