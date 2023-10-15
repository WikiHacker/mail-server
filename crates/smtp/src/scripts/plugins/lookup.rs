/*
 * Copyright (c) 2023 Stalwart Labs Ltd.
 *
 * This file is part of Stalwart Mail Server.
 *
 * This program is free software: you can redistribute it and/or modify
 * it under the terms of the GNU Affero General Public License as
 * published by the Free Software Foundation, either version 3 of
 * the License, or (at your option) any later version.
 *
 * This program is distributed in the hope that it will be useful,
 * but WITHOUT ANY WARRANTY; without even the implied warranty of
 * MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See the
 * GNU Affero General Public License for more details.
 * in the LICENSE file at the top-level directory of this distribution.
 * You should have received a copy of the GNU Affero General Public License
 * along with this program.  If not, see <http://www.gnu.org/licenses/>.
 *
 * You can be released from the requirements of the AGPLv3 license by
 * purchasing a commercial license. Please contact licensing@stalw.art
 * for more details.
*/

use directory::DatabaseColumn;
use sieve::{runtime::Variable, FunctionMap};

use crate::config::scripts::SieveContext;

use super::PluginContext;

pub fn register(plugin_id: u32, fnc_map: &mut FunctionMap<SieveContext>) {
    fnc_map.set_external_function("lookup", plugin_id, 2);
}

pub fn register_map(plugin_id: u32, fnc_map: &mut FunctionMap<SieveContext>) {
    fnc_map.set_external_function("lookup_map", plugin_id, 2);
}

pub fn exec(ctx: PluginContext<'_>) -> Variable {
    let lookup_id = ctx.arguments[0].to_string();
    let span = ctx.span;
    if let Some(lookup) = ctx.core.sieve.lookup.get(lookup_id.as_ref()) {
        match &ctx.arguments[1] {
            Variable::Array(items) => {
                for item in items.iter() {
                    if !item.is_empty()
                        && ctx.handle.block_on(lookup.contains(item)).unwrap_or(false)
                    {
                        return true.into();
                    }
                }
                false
            }
            v if !v.is_empty() => ctx.handle.block_on(lookup.contains(v)).unwrap_or(false),
            _ => false,
        }
    } else {
        tracing::warn!(
            parent: span,
            context = "sieve:lookup",
            event = "failed",
            reason = "Unknown lookup id",
            lookup_id = %lookup_id,
        );
        false
    }
    .into()
}

pub fn exec_map(ctx: PluginContext<'_>) -> Variable {
    let lookup_id = ctx.arguments[0].to_string();
    let items = match &ctx.arguments[1] {
        Variable::Array(l) => l.iter().map(DatabaseColumn::from).collect(),
        v if !v.is_empty() => vec![DatabaseColumn::from(v)],
        _ => vec![],
    };
    let span = ctx.span;

    if !lookup_id.is_empty() && !items.is_empty() {
        if let Some(lookup) = ctx.core.sieve.lookup.get(lookup_id.as_ref()) {
            return ctx
                .handle
                .block_on(lookup.lookup(&items))
                .unwrap_or_default();
        } else {
            tracing::warn!(
                parent: span,
                context = "sieve:lookup",
                event = "failed",
                reason = "Unknown lookup id",
                lookup_id = %lookup_id,
            );
        }
    }

    Variable::default()
}
