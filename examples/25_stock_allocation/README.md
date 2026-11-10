# Stock allocation: a shipment decided by the record

A warehouse allocates stock to a customer's order lines and ships it.
An allocation may take less than the whole item it names. Then the
item splits: a child is created holding what leaves, the parent keeps
the rest, the lineage is recorded, and the allocation is re-pointed at
the child. An allocation that takes the whole item ships the item.

This is the worked example for the conditional statement and for a
`for` over the claims a pattern matches. Shipping is one act, `ship`,
that ranges over the allocations the record holds for the shipment,
`for Allocation(allocation, line, shipment, item, quantity):`, and in
each pass chooses by a condition on the record: `if quantity <
on_hand:` splits, `else:` ships whole. Before it, both lived in the
application: a loop over the allocations, and two acts with the
caller's code deciding which to call. Now the application names the
shipment and the record does the rest.

The rules are what make the split safe to automate. Stock is conserved
across any number of splits, measured against what the item held when
it arrived. A child holds exactly what the lineage says moved. What is
allocated against an item never exceeds what it holds, however an
allocation is later changed. A shipped item is off the shelf. An act
that got any branch wrong would be refused, not stored.

## The programme at a glance

| Claim | Meaning |
|---|---|
| `StockItem(item, part, quantity)` | An item on the shelf and how much it holds. |
| `Received(item, quantity)` | What the item held when it arrived. Kept forever. |
| `InStock(item)` | The item is still on the shelf. |
| `SplitFrom(child, parent, quantity)` | The lineage of a split. Kept forever. |
| `SalesOrder`, `SalesLine` | An order and its lines. |
| `Shipment(shipment, order)`, `ShipmentShipped(shipment)` | A shipment on an order, and that it has gone out. |
| `Allocation(allocation, line, shipment, item, quantity)` | Stock reserved for a line, on a shipment. |
| `ShippedTo(item, customer)` | The item went to the customer. Kept forever. |

| Rule | Says |
|---|---|
| `stock_is_conserved` | What an item holds, plus everything split off it, is what arrived. |
| `a_split_child_holds_what_moved` | A child holds what the lineage says, and is of its parent's part. |
| `allocations_fit_the_stock` | Allocations against an item never exceed what it holds. |
| `lines_are_not_overallocated` | A line is never allocated more than it ordered. |
| `shipped_items_leave_stock` | A shipped item is not in stock. |
| `shipped_allocations_are_settled` | An allocation whose item has shipped reserves exactly what the item holds. |

| Act | Does |
|---|---|
| `allocate` | Reserves part of an item for a line, on a shipment, within the stock and the line. |
| `change_allocation` | Changes the quantity while the shipment is open; the rules refuse a change that no longer fits. |
| `ship` | For every allocation on the shipment, splits the item or ships it whole, by the condition in the act; then marks the shipment gone. |
| `cancel_order` | For every line on the order, removes the line and its allocations; refused if any line has stock already shipped. |

`Allocated(item)` and `LineShipped(line)` are reads: how much of each
item is allocated, and how much of each line has shipped. The second
used to be a figure kept beside the record and moved by the shipping
act; the record can say, so it is read instead.

## What left the application

Choosing between a split and a whole shipment: the condition in the
act. Enumerating the allocations on a shipment, and the lines on an
order: the pattern each act ranges over. Checking that the list the
caller passed was complete: there is no list. Keeping a shipped figure
per line in step with the shipments: it is read from them.

## Run it

```bash
cargo test -p morpholog-examples --test stock_allocation
morpholog check --strict examples/25_stock_allocation/stock_allocation.morph
```

The tests walk a receipt, an order, allocations and a shipment through
both branches, read the figures back after a split, ship a shipment
with several allocations from a record loaded in two orders and get the
same children, cancel an order, and try the changes the rules refuse:
an allocation beyond the stock, a change that would overflow it, a
second allocation of one item on one shipment, shipping twice, changing
an allocation on a shipment that has gone, and cancelling an order with
stock already shipped.

## What it deliberately does not cover

- The same sum, how much of an item is allocated, is written three
  times: in the `allocate` gate, in the `allocations_fit_the_stock`
  rule and in the `Allocated` read. A definition that yields a value
  would make them one.
- A line's part must be the item's part exactly. Variants of a part, a
  tree under it, would need a recursive definition of descent.
- Two allocations on one shipment may not take the same stock item.
  That rule keeps each pass of the shipping act independent of the
  others, since every pass reads the record as it was when the act
  began, and it is simpler than the rule some inventory systems have,
  where the last allocation to visit an item ships the original and
  the earlier ones ship children.
