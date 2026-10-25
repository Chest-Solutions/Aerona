/*
  The one place the theme is decided, and it has to be decided before the first paint.

  `.dark` on <html> is what every shadcn token hangs off, so painting the class first is the
  whole job: a stylesheet that arrives one frame early would otherwise paint the light canvas
  and this script would then flip it, which is the flash this file exists to remove.

  It is a *file* rather than an inline block because the response's Content-Security-Policy is
  `script-src 'self'` with no hashes and no unsafe-inline, and an inline script under that
  policy is not a style choice — it is a blocked request and a panel that hydrates against the
  wrong markup. `src/components/theme-toggle.tsx` owns the same key and the same reading; keep
  the two in step, and `scripts/prerender.mjs` fails the build if this file disappears from the
  build output.
*/
;(function () {
  try {
    var t = localStorage.getItem('aerona.theme') || 'system'
    var dark = t === 'dark' || (t === 'system' && window.matchMedia && matchMedia('(prefers-color-scheme: dark)').matches)
    document.documentElement.classList.toggle('dark', !!dark)
  } catch (e) {
    /* No storage, no preference: the system's own answer is what the stylesheet already says. */
  }
})()
