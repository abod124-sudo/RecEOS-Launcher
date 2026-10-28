// Bug-report relay for the Rec EOS Launcher.
//
// The launcher posts its report here instead of straight to Discord, so the
// webhook address lives only in this Worker's secrets (DISCORD_WEBHOOK) and
// never ships inside the app. The Worker rebuilds every report from a short
// list of allowed fields, limits each IP address, and adds the @everyone ping
// itself, so nobody can use it to post arbitrary messages.

const MAX_BODY = 8 * 1024 * 1024; // Discord refuses uploads over 10 MB.
const MAX_FILE = 3 * 1024 * 1024;
const ALLOWED_FILES = new Set(["logs.txt", "previous-session.txt"]);

// Discord's own limits for an embed.
const LIMITS = { title: 256, description: 4096, fields: 25, fieldName: 256, fieldValue: 1024, footer: 2048 };

function clip(value, max) {
  const text = typeof value === "string" ? value : String(value ?? "");
  return text.length > max ? text.slice(0, max - 1) + "…" : text;
}

// No pings of any kind from text the launcher sent.
function quiet(value, max) {
  return clip(value, max)
    .replace(/@(everyone|here)/gi, "@​$1")
    .replace(/<@[!&]?\d+>/g, "(mention removed)");
}

function reply(status, message) {
  return new Response(JSON.stringify({ message }), {
    status,
    headers: { "content-type": "application/json" },
  });
}

// The embed the launcher builds, with everything else dropped.
function cleanEmbed(embed) {
  if (!embed || typeof embed !== "object") return null;
  const fields = Array.isArray(embed.fields) ? embed.fields.slice(0, LIMITS.fields) : [];
  return {
    title: quiet(embed.title, LIMITS.title),
    description: quiet(embed.description, LIMITS.description),
    color: Number.isInteger(embed.color) ? embed.color & 0xffffff : 0,
    fields: fields
      .filter((f) => f && typeof f === "object")
      .map((f) => ({
        name: quiet(f.name, LIMITS.fieldName) || "-",
        value: quiet(f.value, LIMITS.fieldValue) || "-",
        inline: f.inline === true,
      })),
    footer: { text: quiet(embed.footer?.text, LIMITS.footer) },
  };
}

export async function handle(request, env) {
  const url = new URL(request.url);
  if (url.pathname !== "/report") return reply(404, "Not found.");
  if (request.method !== "POST") return reply(405, "Use POST.");

  const length = Number(request.headers.get("content-length") || 0);
  if (length > MAX_BODY) return reply(413, "The report is too large.");

  // One IP address gets a couple of reports a minute (see wrangler.toml).
  if (env.REPORT_LIMIT) {
    const ip = request.headers.get("cf-connecting-ip") || "unknown";
    const { success } = await env.REPORT_LIMIT.limit({ key: ip });
    if (!success) return reply(429, "Too many reports. Try again in a minute.");
  }

  let form;
  try {
    form = await request.formData();
  } catch {
    return reply(400, "The report could not be read.");
  }

  let payload;
  try {
    payload = JSON.parse(form.get("payload_json"));
  } catch {
    return reply(400, "The report could not be read.");
  }
  const embed = cleanEmbed(Array.isArray(payload?.embeds) ? payload.embeds[0] : null);
  if (!embed || !embed.title) return reply(400, "The report is missing its details.");

  const out = new FormData();
  out.append(
    "payload_json",
    JSON.stringify({
      content: `${quiet(payload.content || "New bug report", 200)} @everyone`,
      allowed_mentions: { parse: ["everyone"] },
      embeds: [embed],
    }),
  );

  let index = 0;
  for (const [key, value] of form.entries()) {
    if (!key.startsWith("files[") || typeof value === "string") continue;
    if (!ALLOWED_FILES.has(value.name) || value.size > MAX_FILE || index >= ALLOWED_FILES.size) {
      return reply(400, "The report has an attachment that isn't allowed.");
    }
    const text = await value.text();
    out.append(`files[${index}]`, new Blob([text], { type: "text/plain" }), value.name);
    index++;
  }

  if (!env.DISCORD_WEBHOOK) return reply(500, "The relay isn't set up yet.");
  let sent;
  try {
    sent = await fetch(env.DISCORD_WEBHOOK, { method: "POST", body: out });
  } catch {
    return reply(502, "Couldn't reach Discord.");
  }
  if (sent.status === 429) return reply(429, "Discord is busy. Try again in a minute.");
  if (!sent.ok) return reply(502, `Discord refused the report (HTTP ${sent.status}).`);
  return new Response(null, { status: 204 });
}

export default { fetch: handle };
