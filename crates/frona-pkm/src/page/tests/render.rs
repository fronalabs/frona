use chrono::{TimeZone, Utc};

use crate::model::KnowledgeMemory;
use crate::model::{AbsoluteTime, Disposition, Episode, EpisodeStatus, MemoryKind, TemporalAnchor};
use crate::page::render::*;

#[test]
fn author_input_includes_episode_status_and_utc_and_local_dates() {
    let memory = KnowledgeMemory {
        id: "episode-1".into(),
        user_id: "user-1".into(),
        created_at: Utc.with_ymd_and_hms(2026, 7, 18, 20, 48, 27).unwrap(),
        kind: MemoryKind::Episodic,
        episode: Some(Episode {
            status: EpisodeStatus::Planned,
            anchor: TemporalAnchor {
                message: "m23".into(),
                quote: String::new(),
            },
            duration: None,
            absolute: Some(AbsoluteTime {
                year: Some(2026),
                month: Some(7),
                day: Some(20),
                hour: Some(2),
                minute: Some(0),
            }),
            resolved_start: Some(Utc.with_ymd_and_hms(2026, 7, 20, 2, 0, 0).unwrap()),
            resolved_end: None,
        }),
        content: "A Sunday-evening reminder was scheduled.".into(),
        relations: Vec::new(),
        disposition: Disposition::None,
        ended_at: None,
        comment: None,
        erroneous_at: None,
        evidence: Vec::new(),
    };

    let rendered = list_or(&[&memory], "(none)", "America/Los_Angeles");

    assert!(rendered.contains("status=planned"), "{rendered}");
    assert!(
        rendered.contains("start_utc=2026-07-20T02:00:00Z"),
        "{rendered}"
    );
    assert!(
        rendered.contains("start_local=2026-07-19T19:00:00-07:00 timezone=America/Los_Angeles"),
        "{rendered}",
    );
}
