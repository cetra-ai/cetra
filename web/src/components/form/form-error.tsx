export function FormError({ message }: { message?: string }) {
  if (!message) {
    return null;
  }
  return (
    <p role="alert" className="text-body text-destructive">
      {message}
    </p>
  );
}
