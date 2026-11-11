Typing this entity as `{{class}}` is **inconsistent** with the ontology — the reasoner
reported:

{{violations}}

Revise your classification so it no longer conflicts: drop the class that does not hold,
pick a more general or a different standard class, or (if you minted a `frona:` class)
fix its parent.

A disjointness clash usually means two of the classes contradict each other, or one
contradicts a fact or a link on this entity; a facet violation means an attribute value
is out of its allowed range.

Nothing from the rejected submission was kept, so **re-send all fields** — `entity`, the
revised `classes`, `relations`, `attributes`, `new_entities`, `declarations`, `has_keys`,
and `inverse_functional_properties` exactly as you had them. Only the reported fields were
in question; anything you leave out this time is simply lost.
