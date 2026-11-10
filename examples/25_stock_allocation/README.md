# Stock allocation: a split decided by the record

A warehouse allocates stock to a customer's order lines and ships it.
An allocation may take less than the whole item it names. Then the
item splits: a child is created holding what leaves, the parent keeps
the rest, the lineage is recorded, and the allocation is re-pointed at
the child. An allocation that takes the whole item ships the item.

This is the worked example for the conditional statement. The choice
between the two changes is made inside one act, `ship_allocation`, by
a condition on the record: `if quantity < on_hand:` splits, `else:`
ships whole. Before it, that choice lived in the application: two acts,
and the caller's code deciding which to call. Now the application calls
one act per allocation and the record decides.

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
| `SalesOrder`, `SalesLine`, `LineShipped` | An order, its lines, and how much of each line has shipped. |
| `Shipment(shipment, order)`, `ShipmentShipped(shipment)` | A shipment on an order, and that it has gone out. |
| `Allocation(allocation, line, shipment, item, quantity)` | Stock reserved for a line, on a shipment. |
| `ShippedTo(item, customer)` | The item went to the customer. Kept forever. |

| Rule | Says |
|---|---|
| `stock_is_conserved` | What an item holds, plus everything split off it, is what arrived. |
| `a_split_child_holds_what_moved` | A child holds what the lineage says, and is of its parent's part. |
| `allocations_fit_the_stock` | Allocations against an item never exceed what it holds. |
| `lines_are_not_overallocated`, `lines_are_not_overshipped` | A line is never allocated or shipped more than it ordered. |
| `shipped_items_leave_stock` | A shipped item is not in stock. |

| Act | Does |
|---|---|
| `allocate` | Reserves part of an item for a line, on a shipment, within the stock and the line. |
| `change_allocation` | Changes the quantity; the rules refuse a change that no longer fits. |
| `ship_allocation` | Splits the item or ships it whole, by the condition in the act. |
| `close_shipment` | Marks the shipment gone once every allocation on it has shipped. |

`Allocated(item)` is a read: how much of each item is allocated.

## What left the application

With one act per allocation, the caller no longer chooses between a
split act and a whole act. The caller still enumerates the allocations
on a shipment and calls `ship_allocation` for each; that loop, and the
act that replaces it, is the next step for this example.

## Run it

```bash
cargo test -p morpholog-examples --test stock_allocation
morpholog check --strict examples/25_stock_allocation/stock_allocation.morph
```

The tests walk a receipt, an order, an allocation and a shipment
through both branches, read the figures back after a split, and try
the changes the rules refuse: an allocation beyond the stock, a change
that would overflow it, a second allocation of one item on one
shipment, shipping twice, and closing a shipment with stock still on
the shelf.

## What it deliberately does not cover

- The same sum, how much of an item is allocated, is written three
  times: in the `allocate` gate, in the `allocations_fit_the_stock`
  rule and in the `Allocated` read. A definition that yields a value
  would make them one.
- A shipment is still shipped one allocation at a time by the caller.
  An act that ranges over the allocations a pattern matches would make
  it one act.
- A line's part must be the item's part exactly. Variants of a part, a
  tree under it, would need a recursive definition of descent.
- Two allocations on one shipment may not take the same stock item.
  That rule keeps each shipping step independent of the order the
  caller visits allocations in, and it is simpler than the rule some
  inventory systems have, where the last allocation to visit an item
  ships the original and the earlier ones ship children.
