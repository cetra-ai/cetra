//! `model_price.*` actions. The target is the price (`target_kind` `model_price`). The scope
//! is the event's org and optional project. Prices are replaced as whole documents, so
//! `updated` carries no field list.

#[derive(Debug, Clone, PartialEq, serde::Serialize)]
#[serde(tag = "action", content = "details")]
pub(crate) enum ModelPriceAction {
    #[serde(rename = "model_price.created")]
    Created,
    #[serde(rename = "model_price.updated")]
    Updated,
    #[serde(rename = "model_price.deleted")]
    Deleted,
}
