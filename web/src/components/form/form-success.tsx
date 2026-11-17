export function FormSuccess({ message }: { message?: string }) {
  if (!message) {
    return null;
  }
  return (
    <p role="status" className="text-body text-muted-foreground">
      {message}
    </p>
  );
}
