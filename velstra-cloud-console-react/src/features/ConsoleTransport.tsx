// A grant describes the API-to-node hop. The browser's WebSocket follows the
// page protocol, so both hops matter before claiming transport encryption.
export function ConsoleTransport({ nodeEncrypted }: { nodeEncrypted: boolean | null }) {
  if (nodeEncrypted === null) return null;
  const browserEncrypted = location.protocol === "https:";
  const encrypted = browserEncrypted && nodeEncrypted;
  const message = encrypted
    ? "Encrypted connection"
    : !browserEncrypted && !nodeEncrypted
      ? "Browser and node connections are not encrypted."
      : !browserEncrypted
        ? "Browser connection is not encrypted."
        : "The connection from the API to this node is not encrypted.";
  return <span role="status" className="text-xs" style={{ color: encrypted ? "var(--settled)" : "var(--pending)" }}>{message}</span>;
}
