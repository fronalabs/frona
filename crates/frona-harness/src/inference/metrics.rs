pub fn set_active_inference_requests(count: usize) {
    metrics::gauge!("frona_inference_active_requests").set(count as f64);
}
