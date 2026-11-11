// Service worker for the installable app.
//
// It caches only the app shell (CSS, JS, icons, offline page) so the app starts
// fast and can show a friendly page when the server is unreachable. Messages,
// attachments and authenticated pages are never cached here: they always come
// from the server, so nothing private is stored on the device by the app.
"use strict";
const VERSION = "{{v}}";
const CACHE = `imsg-shell-${VERSION}`;
const SHELL = [
  "/static/app.css?v={{v}}",
  "/static/app.js?v={{v}}",
  "/offline.html",
  "/static/icons/icon-192.png",
  "/static/icons/icon-512.png",
  "/static/icons/apple-touch-icon.png",
  "/static/icons/favicon-32.png",
];

self.addEventListener("install", (event) => {
  event.waitUntil(caches.open(CACHE).then((c) => c.addAll(SHELL)).then(() => self.skipWaiting()));
});

self.addEventListener("activate", (event) => {
  event.waitUntil(
    caches.keys()
      .then((keys) => Promise.all(keys.filter((k) => k.startsWith("imsg-shell-") && k !== CACHE).map((k) => caches.delete(k))))
      .then(() => self.clients.claim()),
  );
});

self.addEventListener("fetch", (event) => {
  const req = event.request;
  if (req.method !== "GET") return;
  const url = new URL(req.url);
  if (url.origin !== self.location.origin) return;

  if (req.mode === "navigate") {
    // Always go to the network (pages need a live session); fall back offline.
    event.respondWith(fetch(req).catch(() => caches.match("/offline.html")));
    return;
  }
  if (url.pathname.startsWith("/static/")) {
    // Versioned, immutable assets: cache first.
    event.respondWith(
      caches.match(req).then((hit) => hit || fetch(req).then((res) => {
        if (res.ok) {
          const copy = res.clone();
          caches.open(CACHE).then((c) => c.put(req, copy));
        }
        return res;
      })),
    );
  }
  // Everything else (/api, /media, ...) goes straight to the network.
});
