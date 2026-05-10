use crate::models::*;
use wasm_bindgen::prelude::*;

#[wasm_bindgen]
extern "C" {
    #[wasm_bindgen(js_namespace = ["window", "__TAURI__", "core"])]
    async fn invoke(cmd: &str, args: JsValue) -> JsValue;
}

pub async fn list_roadmap_features(project_id: String) -> Result<Vec<RoadmapFeature>, String> {
    let args = serde_wasm_bindgen::to_value(&serde_json::json!({ "projectId": project_id })).unwrap();
    let response = invoke("list_roadmap_features", args).await;
    serde_wasm_bindgen::from_value(response).map_err(|e| e.to_string())
}

pub async fn delete_roadmap_feature(feature_id: String) -> Result<(), String> {
    let args = serde_wasm_bindgen::to_value(&serde_json::json!({ "featureId": feature_id })).unwrap();
    let response = invoke("delete_roadmap_feature", args).await;
    serde_wasm_bindgen::from_value(response).map_err(|e| e.to_string())
}

pub async fn update_roadmap_feature_status(feature_id: String, status: RoadmapStatus) -> Result<Option<RoadmapFeature>, String> {
    let args = serde_wasm_bindgen::to_value(&serde_json::json!({
        "featureId": feature_id,
        "request": { "status": status },
    })).unwrap();
    let response = invoke("update_roadmap_feature", args).await;
    serde_wasm_bindgen::from_value(response).map_err(|e| e.to_string())
}

pub async fn convert_to_task(feature_id: String) -> Result<Task, String> {
    let args = serde_wasm_bindgen::to_value(&serde_json::json!({ "featureId": feature_id })).unwrap();
    let response = invoke("convert_to_task", args).await;
    serde_wasm_bindgen::from_value(response).map_err(|e| e.to_string())
}

pub async fn run_competitor_analysis(
    req: CompetitorAnalysisRequest,
) -> Result<CompetitorAnalysis, String> {
    let args = serde_wasm_bindgen::to_value(&serde_json::json!({
        "featureId": req.feature_id,
        "competitors": req.competitors,
    })).unwrap();

    let response = invoke("run_competitor_analysis", args).await;
    serde_wasm_bindgen::from_value(response).map_err(|e| e.to_string())
}
