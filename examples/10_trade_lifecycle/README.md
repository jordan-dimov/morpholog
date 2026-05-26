# Trade lifecycle

A commodity trade does not have a status. It has a history.

When a trading desk strikes a deal, it travels a path: a trader *captures*
it, the middle office *confirms* it against the counterparty and fixes an
official price, and in time it *settles*. Most systems record where a trade
is on that path in a single mutable field - `status = "confirmed"` - and
then spend the next three years reconstructing, from logs and emails, what
that field used to say and who was allowed to change it.

This example takes the other view. A trade is "captured" because a capture
claim exists for it; "confirmed" because a confirmation claim exists;
"settled" because a settlement claim exists. The phase is the accumulation
of admitted claims, and earlier claims never stop being true - so the
history reads itself, with no status column to overwrite and no audit
reconstruction to perform.

Two controls carry the weight, and they answer different questions:

- **Who may confirm a price** is a question settled at the moment someone
  acts. It belongs in a gate (`require`): confirming a trade today stays
  valid even if that authority is withdrawn tomorrow.
- **A trade may never be settled for more than the quantity captured** is a
  question that must hold for all admitted state, forever. It belongs in an
  invariant: no path, however the books are reached, may leave an
  over-settled trade behind.

The trade also carries two prices at two standings. The trader's captured
price is an estimate - recorded, but not something you may settle on. The
middle office's confirmed price is the official one, and settlement relies
on *that*. When the official price is later corrected, the correction
governs future settlements; a settlement already made under the prior
official price stays a true record of what was settled that day.

This is the first worked example built as Morpholog's first external
embedder (an ETRM teaching simulator); it is assembled over a series of
commits, beginning with the capture step.
