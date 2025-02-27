// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

use super::{big_int::BigInt, move_type::MoveType};
use crate::types::owner::Owner;
use async_graphql::*;

#[derive(Clone, Debug, PartialEq, Eq, SimpleObject)]
pub(crate) struct Balance {
    /// Coin type for the balance, such as 0x2::sui::SUI
    pub(crate) coin_type: Option<MoveType>,
    /// How many coins of this type constitute the balance
    pub(crate) coin_object_count: Option<u64>,
    /// Total balance across all coin objects of the coin type
    pub(crate) total_balance: Option<BigInt>,
}

#[derive(Clone, Debug, PartialEq, Eq, SimpleObject)]
pub(crate) struct BalanceChange {
    pub(crate) owner: Option<Owner>,
    pub(crate) amount: Option<BigInt>,
    pub(crate) coin_type: Option<MoveType>,
}
