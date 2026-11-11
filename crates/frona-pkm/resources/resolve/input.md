Subject path: {{path}}
Mention: {{name}} — {{description}}
Aliases: {{aliases}}
Type: {{kind}}
Grounded identity evidence: {{identity_evidence}}
Assertions: {{assertions}}

Identity candidates (provably disjoint types already removed):
{{candidates}}

Is the subject the SAME entity as one or more candidates? Put the surviving subject or
candidate in `canonical` and every losing candidate in `same_as`. If the subject is
distinct, return an empty `canonical` and an empty `same_as` array. Include grounded
`merge_because` evidence for every merge and grounded `distinct_because` evidence for
every strong declined candidate. Re-send all four keys.
