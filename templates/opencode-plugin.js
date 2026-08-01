const destructive = /\b(rm\s+-[rf]|rmdir|truncate|mkfs|dd\s+if=|git\s+(reset\s+--hard|clean\s+-[fdx])|drop\s+(table|database))\b/i;
const publish = /\bgit\s+push\b|\bgh\s+(pr|repo)\b|\bgit\s+remote\b/i;

function classify(event) {
  const kind = event.type ?? "";
  if (kind !== "permission.asked" && kind !== "tool.execute.before") return null;
  const body = JSON.stringify(event.properties ?? event);
  if (destructive.test(body)) return "destructive action";
  if (publish.test(body) && event.properties?.preApproved !== true) return "unapproved publish";
  return null;
}

async function notify(title, body) {
  const ntfy = process.env.OCA_NTFY_URL;
  if (ntfy) await fetch(ntfy, { method: "POST", body: `${title}: ${body}` });
  if (process.env.OCA_DESKTOP_NOTIFY === "1")
    Bun.spawn(["notify-send", title, body], { stdout: "ignore", stderr: "ignore" });
}

export default async function OcaNotify() {
  return {
    event: async ({ event }) => {
      const reason = classify(event);
      if (!reason) return;
      await notify(`oca ${reason}`, `session=${event.properties?.sessionID ?? "unknown"}`);
    },
  };
}
