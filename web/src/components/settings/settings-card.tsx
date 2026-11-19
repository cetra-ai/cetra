import { type FormEvent, type ReactNode, useId } from "react";
import { DisabledReason } from "@/components/disabled-reason";
import { FormError } from "@/components/form/form-error";
import { FormSuccess } from "@/components/form/form-success";
import { SubmitButton } from "@/components/form/submit-button";
import { UnsavedChangesGuard } from "@/components/settings/unsaved-changes";

const CARD_CLASSES = "rounded-lg border bg-card text-card-foreground";

/** A bordered group of settings with an optional title, actions and footer. */
export function SettingsCard({
  title,
  description,
  actions,
  children,
  footer,
  tone,
  className,
}: {
  title?: string;
  description?: ReactNode;
  /** Top-right of the card header. */
  actions?: ReactNode;
  children: ReactNode;
  footer?: ReactNode;
  tone?: "danger";
  className?: string;
}) {
  const titleId = useId();
  const danger = tone === "danger";
  const hasHeader = title !== undefined || description !== undefined || actions !== undefined;
  return (
    <section
      aria-labelledby={title === undefined ? undefined : titleId}
      className={`${CARD_CLASSES} ${danger ? "border-destructive/50" : ""} ${className ?? ""}`}
    >
      {hasHeader ? (
        <div className="flex items-start justify-between gap-4 px-5 pt-4">
          <div className="flex flex-col gap-1">
            {title === undefined ? null : (
              <h2
                id={titleId}
                className={`text-ui font-medium ${danger ? "text-destructive" : ""}`}
              >
                {title}
              </h2>
            )}
            {description === undefined ? null : (
              <div className="text-body text-muted-foreground">{description}</div>
            )}
          </div>
          {actions === undefined ? null : <div className="shrink-0">{actions}</div>}
        </div>
      ) : null}
      <div className="flex flex-col gap-4 px-5 py-4">{children}</div>
      {footer === undefined ? null : (
        <div
          className={`flex items-center justify-end gap-3 border-t bg-muted/30 px-5 py-3 ${
            danger ? "border-destructive/30" : ""
          }`}
        >
          {footer}
        </div>
      )}
    </section>
  );
}

/**
 * A settings card whose body is a form. Save sits in the footer and is enabled only while the
 * values are dirty and valid. Navigating away with unsaved edits asks first.
 */
export function SettingsFormCard({
  title,
  description,
  ariaLabel,
  children,
  onSubmit,
  dirty,
  invalid,
  pending,
  saved,
  error,
  disabledReason,
  saveLabel = "Save",
  footerExtra,
}: {
  title: string;
  description?: ReactNode;
  /** Accessible name of the form. Defaults to `title`. */
  ariaLabel?: string;
  children: ReactNode;
  onSubmit: () => void;
  dirty: boolean;
  invalid?: boolean;
  pending: boolean;
  /** Shows "Saved" in the footer after a save, until the values change again. */
  saved?: boolean;
  error?: string;
  /** Role or plan reason: Save is disabled and this text explains why. */
  disabledReason?: string;
  saveLabel?: string;
  /** For example an upgrade button, left of the status. */
  footerExtra?: ReactNode;
}) {
  const titleId = useId();
  function handleSubmit(event: FormEvent<HTMLFormElement>) {
    event.preventDefault();
    onSubmit();
  }
  return (
    <form
      noValidate
      aria-label={ariaLabel ?? title}
      aria-labelledby={ariaLabel === undefined ? titleId : undefined}
      onSubmit={handleSubmit}
      className={CARD_CLASSES}
    >
      <div className="flex flex-col gap-1 px-5 pt-4">
        <h2 id={titleId} className="text-ui font-medium">
          {title}
        </h2>
        {description === undefined ? null : (
          <div className="text-body text-muted-foreground">{description}</div>
        )}
      </div>
      <div className="flex flex-col gap-4 px-5 py-4">{children}</div>
      <div className="flex items-center justify-end gap-3 border-t bg-muted/30 px-5 py-3">
        <div className="mr-auto flex items-center gap-2">
          {footerExtra}
          {error ? (
            <FormError message={error} />
          ) : saved && !dirty ? (
            <FormSuccess message="Saved" />
          ) : dirty ? (
            <span className="text-caption text-muted-foreground">Unsaved changes</span>
          ) : null}
        </div>
        <DisabledReason reason={disabledReason}>
          <SubmitButton
            pending={pending}
            disabled={!dirty || invalid === true || pending || disabledReason !== undefined}
          >
            {saveLabel}
          </SubmitButton>
        </DisabledReason>
      </div>
      <UnsavedChangesGuard dirty={dirty && !pending} />
    </form>
  );
}
