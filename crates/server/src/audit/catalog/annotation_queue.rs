//! `annotation_queue.*` actions. The target is the queue (`target_kind` `annotation_queue`).
//! Queue items are not recorded. Queues are replaced as whole documents, so `updated`
//! carries no field list.

#[derive(Debug, Clone, PartialEq, serde::Serialize)]
#[serde(tag = "action", content = "details")]
pub(crate) enum AnnotationQueueAction {
    #[serde(rename = "annotation_queue.created")]
    Created,
    #[serde(rename = "annotation_queue.updated")]
    Updated,
    #[serde(rename = "annotation_queue.deleted")]
    Deleted,
}
