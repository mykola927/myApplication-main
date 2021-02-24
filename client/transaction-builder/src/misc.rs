// Copyright (c) The Diem Core Contributors
// SPDX-License-Identifier: Apache-2.0

#![forbid(unsafe_code)]

use compiled_stdlib::StdLibOptions;
use diem_types::{
    access_path::AccessPath,
    transaction::ChangeSet,
    write_set::{WriteOp, WriteSetMut},
};

// Update WriteSet
pub fn encode_stdlib_upgrade_transaction(option: StdLibOptions) -> ChangeSet {
    let mut write_set = WriteSetMut::new(vec![]);
    let stdlib_modules = compiled_stdlib::stdlib_modules(option);
    let bytes = stdlib_modules.bytes_vec();
    for (module, bytes) in stdlib_modules.compiled_modules.iter().zip(bytes) {
        write_set.push((
            AccessPath::code_access_path(module.self_id()),
            WriteOp::Value(bytes),
        ));
    }
    ChangeSet::new(
        write_set.freeze().expect("Failed to create writeset"),
        vec![],
    )
}
