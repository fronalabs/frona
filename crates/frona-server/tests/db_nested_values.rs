use serde_json::{Value, json};
use surrealdb::{Surreal, engine::local::Mem};

// Tool schemas and execution histories contain nested objects and arrays.
// Exercise the embedded database codec on the standard test-thread stack.
#[tokio::test]
async fn nested_execution_metadata_round_trips_on_default_stack() {
    let db = Surreal::new::<Mem>(()).await.unwrap();
    db.use_ns("test").use_db("nested_history").await.unwrap();
    let mut payload = json!({"text": "tool result"});
    for _ in 0..48 {
        payload = json!({"items": [payload]});
    }
    db.query("CREATE history:one SET payload = $payload")
        .bind(("payload", payload.clone()))
        .await
        .unwrap()
        .check()
        .unwrap();
    let mut response = db
        .query("SELECT VALUE payload FROM ONLY history:one")
        .await
        .unwrap();
    let stored: Option<Value> = response.take(0).unwrap();
    assert_eq!(stored, Some(payload));
}
