# Sealed Bids

Commit now, reveal later, and let the record refuse a reveal that differs from what was committed.

## The scenario

An auctioneer offers a lot with a closing instant. Before the close, each bidder hands in a sealed envelope: a commitment to their bid, computed as a digest of the bid and a secret salt. After the close, each bidder opens the envelope by revealing the bid and the salt. The record recomputes the digest and refuses a reveal that does not match.

What this governs: a bidder cannot change a bid after seeing the others, and an auctioneer cannot leak one, because nobody holds a bid before the close. What it adds over an application that stores a hash: the check is a rule of the record, so no act can admit a mismatched reveal, whoever runs it.

## The programme

[`sealed_bids.morph`](sealed_bids.morph) teaches the domain and the reasoning at each construct.

| Claims | |
| --- | --- |
| `Lot(lot, auctioneer, closes_at)` | An offered lot. Append only. |
| `Sealed(lot, bidder, commitment)` | An envelope, one per bidder per lot. Append only. |
| `Closed(lot)` | The lot has closed. Append only. |
| `Bid(lot, bidder, amount, salt)` | An opened envelope, one per bidder per lot. Append only. |
| `LeadingBid(lot, bidder, amount)` | A view: the highest revealed bid so far. A leader, not a winner. |

| Rules | |
| --- | --- |
| `a_revealed_bid_hashes_to_its_commitment` | `digest(#sealed_bid_v1, lot, bidder, amount, salt) = commitment` |
| `a_bid_is_of_a_sealed_envelope` | No bid without its envelope. |
| `a_bid_stands_on_a_closed_lot` | No bid on an open lot. |
| `an_envelope_is_for_an_offered_lot` | No envelope for a lot nobody offered. |

| Acts | |
| --- | --- |
| `offer(lot, closes_at)` | The actor becomes the auctioneer. |
| `seal(lot, commitment)` | Before the deadline, while the lot is not closed. The bidder is the actor. |
| `close(lot)` | The auctioneer, once the deadline has passed. |
| `reveal(lot, amount, salt)` | On a closed lot, under the bidder's own name. |

## The commitment

The host computes the commitment before the close with the same encoding the kernel uses. In the generated Python client:

```python
from morpholog_client import digest

commitment = digest("sealed_bid_v1", lot, bidder, Decimal("100.00"), salt)
```

The digest hashes the stored form of each value, so `Decimal("100.00")` and `Decimal("100")` commit alike. The salt is the bidder's own randomness; the record never makes one.

Changing what goes into the envelope, or the order of it, is a protocol migration: a later programme names a new protocol literal, and an envelope sealed under the old one must still be revealable under the old rule. Rehearse such a change against the live record before promoting it, as any model change is.

## Run it

```sh
cargo test -p morpholog-examples --test sealed_bids
```

## Not covered

A reveal deadline and an award. A bid can still be revealed after the close, so the view is a leader. Proxies: the bidder is always the actor. A bytes kind: the commitment and the salt are subjects.
