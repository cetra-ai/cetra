-- Applied project and org drops, oldest first, for the control plane (D-P-49).
CREATE INDEX deletions_applied_drops ON deletions(applied_at, id)
  WHERE kind IN ('project', 'org') AND status = 'applied';
