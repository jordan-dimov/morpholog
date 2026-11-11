# Stock allocation: a shipment decided by the record

A warehouse reserves stock for a customer's order lines and ships it.
An allocation may take part of an item. Then the item splits: a child
holds what leaves, the parent keeps the rest, and the allocation now
names the child. An allocation that takes the whole item ships the
item.

Shipping is one act, `ship`. It visits every allocation on the
shipment and, for each, chooses: split or ship whole. Before, the
application did both: it listed the allocations and picked one of two
acts for each. Now it names the shipment.

The rules make the split safe. Stock is conserved across any number of
splits. A child holds what the lineage says. Allocations never exceed
the stock. A shipped item is off the shelf. An act that got any of this
wrong is refused, not stored.

This example carries four language features: the `if` statement, a
`for` over the claims a pattern matches, a definition that names a
figure, and a parameter that declares its kind.

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
| `MayCancelShipped(person)` | Who may cancel an order after stock has shipped. No act grants it; it arrives with the deployment. |

| Rule | Says |
|---|---|
| `stock_is_conserved` | What an item holds, plus everything split off it, is what arrived. |
| `a_split_child_holds_what_moved` | A child holds what the lineage says, and is of its parent's part. |
| `allocations_fit_the_stock` | Allocations against an item never exceed what it holds. |
| `lines_are_not_overallocated` | A line is never allocated more than it ordered. |
| `shipped_items_leave_stock` | A shipped item is not in stock. |
| `shipped_allocations_are_settled` | Once its item has shipped, an allocation reserves exactly what the item holds. |

| Act | Does |
|---|---|
| `allocate` | Reserves part of an item for a line, on a shipment. |
| `change_allocation` | Changes the quantity while the shipment is open. |
| `ship` | Ships every allocation on the shipment, splitting or whole, and marks the shipment gone. |
| `cancel_order` | Removes the order, its lines and their allocations. Refused once stock has shipped, unless the caller sets `force` and may. |

`allocated(item)` is one figure, defined once: how much of an item is
reserved. The `allocate` gate, the `allocations_fit_the_stock` rule and
the `Allocated(item)` view all read it. `LineShipped(line)` is a view
of how much of each line has shipped.

## What left the application

- The choice between a split and a whole shipment. It is an `if` in the act.
- The loop over the allocations on a shipment, and over the lines on an order. Each act ranges over a pattern.
- The check that a list passed in was complete. There is no list.
- A running figure of what each line has shipped. The record can say.
- The type of the `force` flag. No rule reads it into a claim, so `force: Bool` declares it, and the schema and the client carry it typed.

## Run it

```bash
cargo test -p morpholog-examples --test stock_allocation
morpholog check --strict examples/25_stock_allocation/stock_allocation.morph
```

The tests ship through both branches and read the figures back. They
ship a record loaded in two orders and get the same children. They
cancel an order, plain and over shipped stock. And they try what the
rules refuse: an allocation beyond the stock or the line, a second
allocation of one item on one shipment, shipping twice, a change after
shipping, cancelling over shipped stock without the authority, and a
flag that is not a Boolean.

## What it deliberately does not cover

- A line's part must be the item's part exactly. A tree of variants
  under a part would need a recursive definition of descent.
- Two allocations on one shipment may not take the same item. That
  keeps each pass of `ship` independent of the others, since every pass
  reads the record as it was when the act began.
