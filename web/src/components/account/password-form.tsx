import { useState } from "react";
import { $api } from "@/api/client";
import { fieldErrors, problemMessage, problemOf } from "@/api/problem";
import { RATE_LIMITED } from "@/components/auth/messages";
import {
  PasswordFields,
  type PasswordProblems,
  passwordProblems,
} from "@/components/auth/password-fields";
import { FormSuccess } from "@/components/form/form-success";
import { TextField } from "@/components/form/text-field";
import { SettingsFormCard } from "@/components/settings/settings-card";

export function PasswordForm() {
  // gcTime 0: the request carries the current password, so its variables must leave
  // the mutation cache as soon as nothing observes it (README § Pitfalls).
  const change = $api.useMutation("post", "/api/v1/me/password", { gcTime: 0 });
  const [current, setCurrent] = useState("");
  const [next, setNext] = useState("");
  const [confirm, setConfirm] = useState("");
  // A client error shows once its field was typed in or a submit was tried, and then follows the values.
  const [nextTouched, setNextTouched] = useState(false);
  const [confirmTouched, setConfirmTouched] = useState(false);
  const [submitted, setSubmitted] = useState(false);
  const [done, setDone] = useState(false);

  const problems: PasswordProblems = passwordProblems(next, confirm);
  const invalid = problems.password !== undefined || problems.confirm !== undefined;
  const dirty = current.length > 0 || next.length > 0 || confirm.length > 0;
  const clientErrors: PasswordProblems = {
    password: submitted || nextTouched ? problems.password : undefined,
    confirm: submitted || confirmTouched ? problems.confirm : undefined,
  };

  const problem = problemOf(change.error);
  const serverFields = fieldErrors(change.error);
  const currentError =
    serverFields.current_password ??
    (problem?.status === 403 ? "Current password is incorrect." : undefined);
  const formError =
    change.error && currentError === undefined && serverFields.new_password === undefined
      ? problem?.status === 429
        ? RATE_LIMITED
        : problemMessage(change.error, "Could not change your password. Try again.")
      : undefined;

  function onSubmit() {
    setDone(false);
    // A stale server error must not sit next to a new client error.
    change.reset();
    setSubmitted(true);
    if (invalid) {
      return;
    }
    change.mutate(
      {
        // The current password is sent untrimmed: passwords are never trimmed.
        body:
          current.length > 0
            ? { current_password: current, new_password: next }
            : { new_password: next },
      },
      {
        onSuccess: () => {
          // The form stays mounted, so the passwords must leave the mutation's
          // `variables` here; `gcTime: 0` alone would keep them while it is observed.
          change.reset();
          setCurrent("");
          setNext("");
          setConfirm("");
          setNextTouched(false);
          setConfirmTouched(false);
          setSubmitted(false);
          setDone(true);
        },
      },
    );
  }

  return (
    <SettingsFormCard
      title="Password"
      description="Changing it signs out your other sessions and personal access tokens."
      onSubmit={onSubmit}
      dirty={dirty}
      invalid={invalid}
      pending={change.isPending}
      error={formError}
      saveLabel="Change password"
    >
      <TextField
        id="password-current"
        label="Current password (optional)"
        type="password"
        autoComplete="current-password"
        value={current}
        description="Leave empty if you have never set a password (single sign-on accounts)."
        error={currentError}
        disabled={change.isPending}
        onChange={(value) => {
          setCurrent(value);
          setDone(false);
        }}
      />
      <PasswordFields
        password={next}
        confirm={confirm}
        onPasswordChange={(value) => {
          setNext(value);
          setNextTouched(true);
          setDone(false);
        }}
        onConfirmChange={(value) => {
          setConfirm(value);
          setConfirmTouched(true);
          setDone(false);
        }}
        passwordLabel="New password"
        passwordError={clientErrors.password ?? serverFields.new_password}
        confirmError={clientErrors.confirm}
        disabled={change.isPending}
      />
      <FormSuccess
        message={
          done
            ? "Password changed. Your other sessions and personal access tokens were signed out."
            : undefined
        }
      />
    </SettingsFormCard>
  );
}
