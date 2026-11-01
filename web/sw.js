// Keeps the page and the card pictures on the phone, so the cards still flick through when the computer is
// off or there is no connection. The page itself fills the card cache (see index.html); this only serves it.
"use strict";
const SHELL = "shell-v1";
const CARDS = "cards-v1";
const SHELL_FILES = ["/", "/manifest.webmanifest", "/icon.png"];

self.addEventListener("install", (e) => {
  e.waitUntil(caches.open(SHELL).then((c) => c.addAll(SHELL_FILES)).then(() => self.skipWaiting()));
});

self.addEventListener("activate", (e) => {
  e.waitUntil((async () => {
    for (const k of await caches.keys()) if (k !== SHELL && k !== CARDS) await caches.delete(k);
    await self.clients.claim();
  })());
});

self.addEventListener("fetch", (e) => {
  const req = e.request;
  if (req.method !== "GET") return;
  const url = new URL(req.url);
  if (url.origin !== location.origin || url.pathname.startsWith("/api/")) return;
  if (url.pathname === "/img") {
    // Pictures: the saved copy first, the computer only when there is none.
    e.respondWith(caches.open(CARDS).then((c) => c.match(req)).then((hit) => hit || fetch(req)));
    return;
  }
  e.respondWith(networkFirst(req));
});

// The page, manifest and icon: the computer first (so an update shows at once), the saved copy when it
// does not answer within 4 seconds.
async function networkFirst(req) {
  const cache = await caches.open(SHELL);
  try {
    const res = await Promise.race([
      fetch(req),
      new Promise((_, reject) => setTimeout(() => reject(new Error("slow")), 4000)),
    ]);
    if (res.ok) cache.put(req, res.clone());
    return res;
  } catch (err) {
    const hit = await cache.match(req, { ignoreSearch: true });
    if (hit) return hit;
    throw err;
  }
}
