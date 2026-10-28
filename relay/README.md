# Bug-report relay

A Cloudflare Worker that passes bug reports from the launcher to Discord.
The Discord webhook address is stored only as a Worker secret, so it is never
inside the app. The Worker rebuilds each report from allowed fields, accepts
only `logs.txt` and `previous-session.txt` attachments, allows 2 reports per IP
address per minute, and adds the `@everyone` ping itself.

## Deploy

From this folder:

```
npx wrangler login
npx wrangler deploy
npx wrangler secret put DISCORD_WEBHOOK
```

Paste the new webhook address when asked. `deploy` prints the Worker's
address; the launcher's `REPORT_URL` in `src-tauri/src/bugreport.rs` must be
that address followed by `/report`.

## If the webhook leaks again

Delete it in Discord, create a new one, and run `npx wrangler secret put
DISCORD_WEBHOOK` again. The app doesn't need a new release.
