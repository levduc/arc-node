// Copyright 2026 Circle Internet Group, Inc. All rights reserved.
//
// SPDX-License-Identifier: Apache-2.0
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//      http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! The LEAN payment lane's consensus-layer logic, in one place.
//!
//! Everything here is reached only when `ARC_PAYMENT_LEAN_LANE=1` put a
//! `LeanShim` in the handlers' hands; with the flag off the handlers pass
//! `None` and none of this runs. Keeping it out of the handler bodies is what
//! makes each flag-gated arm a single call, and what makes the lane portable:
//! the delta against upstream is this directory plus one call per arm.

pub(crate) mod anchor;
