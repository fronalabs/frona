#[derive(Debug, Default, Clone, serde::Serialize, serde::Deserialize)]
pub struct ResearchCoverageStats {
    pub messages: usize,
    pub extracted: usize,
    pub no_durable_claim: usize,
    pub duplicate: usize,
    pub unsupported: usize,
    pub memories_added_by_repair: usize,
    pub citation_repairs: usize,
    pub mixed_claim_splits: usize,
    pub claims: usize,
    pub claims_extracted: usize,
    pub claims_no_durable_claim: usize,
    pub claims_duplicate: usize,
    pub claims_unsupported: usize,
}

impl ResearchCoverageStats {
    pub fn add(&mut self, other: &Self) {
        self.messages += other.messages;
        self.extracted += other.extracted;
        self.no_durable_claim += other.no_durable_claim;
        self.duplicate += other.duplicate;
        self.unsupported += other.unsupported;
        self.memories_added_by_repair += other.memories_added_by_repair;
        self.citation_repairs += other.citation_repairs;
        self.mixed_claim_splits += other.mixed_claim_splits;
        self.claims += other.claims;
        self.claims_extracted += other.claims_extracted;
        self.claims_no_durable_claim += other.claims_no_durable_claim;
        self.claims_duplicate += other.claims_duplicate;
        self.claims_unsupported += other.claims_unsupported;
    }
}
