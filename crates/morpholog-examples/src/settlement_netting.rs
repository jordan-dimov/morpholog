//! Settlement netting example IR.
//!
//! Surface-syntax form: `examples/01_settlement_netting/netting.morph`.

use morpholog_core::{Invariant, Transformation};

use morpholog_core::dsl::*;

pub fn net_settlement_has_lines() -> Invariant {
    Invariant {
        name: "net_settlement_has_lines".to_string(),
        version: 1,
        body: implies(
            claim(
                "NetSettlement",
                vec![var("net"), wildcard(), wildcard(), wildcard()],
            ),
            exists(
                "line",
                claim(
                    "SettlementLine",
                    vec![var("line"), var("net"), wildcard()],
                ),
            ),
        ),
    }
}

pub fn net_amount_equals_lines() -> Invariant {
    Invariant {
        name: "net_amount_equals_lines".to_string(),
        version: 1,
        body: implies(
            claim(
                "NetSettlement",
                vec![var("net"), wildcard(), wildcard(), var("amount")],
            ),
            eq(
                term(var("amount")),
                sum(
                    var("x"),
                    "x",
                    claim("SettlementLine", vec![wildcard(), var("net"), var("x")]),
                ),
            ),
        ),
    }
}

pub fn no_double_netting() -> Invariant {
    Invariant {
        name: "no_double_netting".to_string(),
        version: 1,
        body: implies(
            claim(
                "SettlementLine",
                vec![var("line"), var("net"), wildcard()],
            ),
            not(exists(
                "other",
                and(vec![
                    claim(
                        "SettlementLine",
                        vec![var("line"), var("other"), wildcard()],
                    ),
                    neq(var("other"), var("net")),
                ]),
            )),
        ),
    }
}

pub fn create_net_settlement() -> Transformation {
    Transformation {
        name: "create_net_settlement".to_string(),
        parameters: params(&["party_a", "party_b", "lines"]),
        body: vec![
            require(forall(
                "line",
                in_(var("line"), var("lines")),
                and(vec![
                    claim("ApprovedSettlementLine", vec![var("line")]),
                    claim("Between", vec![var("line"), var("party_a"), var("party_b")]),
                    not(claim("Netted", vec![var("line")])),
                ]),
            )),
            let_new_subject("net"),
            let_(
                "amount",
                sum(
                    var("x"),
                    "x",
                    and(vec![
                        in_(var("line"), var("lines")),
                        claim("LineAmount", vec![var("line"), var("x")]),
                    ]),
                ),
            ),
            assert_(
                "NetSettlement",
                vec![var("net"), var("party_a"), var("party_b"), var("amount")],
            ),
            for_(
                "line",
                term(var("lines")),
                vec![
                    let_(
                        "amt",
                        value_of("LineAmount", vec![var("line"), wildcard()]),
                    ),
                    assert_("SettlementLine", vec![var("line"), var("net"), var("amt")]),
                    assert_("Netted", vec![var("line")]),
                ],
            ),
            emit("NetSettlementCreated", vec![var("net")]),
        ],
    }
}

pub fn all_invariants() -> Vec<Invariant> {
    vec![
        net_settlement_has_lines(),
        net_amount_equals_lines(),
        no_double_netting(),
    ]
}

/// The settlement-netting example as a [`morpholog_core::Program`]: one
/// transformation (`create_net_settlement`) and three invariants.
/// Stable identifier: `"settlement_netting"`.
pub fn program() -> morpholog_core::Program {
    morpholog_core::Program {
        name: "settlement_netting".to_string(),
        invariants: all_invariants(),
        transformations: vec![create_net_settlement()],
        derived_claims: vec![],
    }
}
