// Pre-paint bootstrap: applies the saved skin, the Liquid Glass effect and the
// animation and network classes before the stylesheet renders, so the window
// never flashes the wrong look. Lives in its own file rather than inline so the
// CSP can forbid inline script entirely.
//
// Everything here is replayed from localStorage, which the real applyTheme()
// writes on every change. The config only arrives over IPC a moment later; until
// it does, this is all the launcher knows about how it should look.
try {
  // Only a skin name, and only one that could have come from the dropdown:
  // localStorage is hand-editable, and a stray value must not be able to stamp
  // an arbitrary class onto <body>.
  var savedTheme = localStorage.getItem('radium-theme');

  // Glass: three values, not a stylesheet. The sheet itself is
  // skins/11-glass.css, linked in index.html and already parsed by the time
  // this runs — all that is left is to say which tint, which backdrop and
  // whether full effects are on. This used to inject an ~80 KB generated
  // stylesheet (with the backdrop's data: URI inside it) read back out of
  // localStorage on every start.
  var glass = null;
  try {
    glass = JSON.parse(localStorage.getItem('radium-glass') || 'null');
  } catch (e) {}

  if (glass && typeof glass === 'object') {
    // Glass stands in for the skin entirely — it repaints every colour token
    // and brings its own layout — so it is applied instead of the skin class,
    // exactly as applyTheme() does once the config lands.
    document.body.classList.add('theme-moderndark', 'glass-enabled');

    // Re-checked here rather than trusted: this came from storage, and the
    // values go straight into CSS. Mirrors safeColor()/safeBackdrop() in
    // app.js, which is what wrote them.
    if (/^#[0-9a-fA-F]{3,8}$/.test(String(glass.tint || ''))) {
      document.body.style.setProperty('--lg-tint', glass.tint);
    }
    // A stored picture only: the CSP lets the page load no remote image, so an
    // address would paint nothing and leave the window see-through. Mirrors
    // paintableBackdrop() in app.js.
    var bg = String(glass.bgImage || '');
    var shapeOk = bg.indexOf('data:image/') === 0;
    if (shapeOk && !/['"(){}\\]|[\x00-\x1f\x7f]/.test(bg)) {
      document.body.style.setProperty('--lg-backdrop', 'url("' + bg + '")');
      document.body.classList.add('glass-has-image');
    }
    if (glass.fullEffects !== false) document.body.classList.add('glass-full');
  } else if (savedTheme && /^[a-z0-9-]+$/.test(savedTheme)) {
    // Modern Neon Dark is drawn under its own class, because theme-moderndark
    // is Liquid Glass's layout. Mirrors skinClass() in app.js.
    document.body.classList.add('theme-' + (savedTheme === 'moderndark' ? 'neondark' : savedTheme));
  }

  // Always on: the setting that could turn animations off has been removed.
  document.body.classList.add('animations-enabled');
  // Network brand, applied before paint for the same reason as the skin.
  var savedNetwork = localStorage.getItem('radium-network');
  var network = (savedNetwork === 'vanilla' || savedNetwork === 'stella' || savedNetwork === 'radium') ? savedNetwork : 'receos';
  document.body.classList.add('network-' + network);
  // That network's Home banner, when the user picked a picture for it.
  // Checked like the glass backdrop: a stored picture only, nothing that could
  // end the url(). Mirrors applyHomeBanner() in app.js, which wrote it.
  try {
    var banner = JSON.parse(localStorage.getItem('radium-home-banner') || 'null');
    var image = banner && banner.network === network ? String(banner.image || '') : '';
    if (image.indexOf('data:image/') === 0 && !/['"(){}\\]|[\x00-\x1f\x7f]/.test(image)) {
      document.body.style.setProperty('--home-banner', 'url("' + image + '")');
      document.body.classList.add('has-home-banner');
    }
  } catch (e) {}
  // Home's layout (Settings → Home → Style). A known name only, like the skin.
  // Mirrors applyHomeLayout() in app.js, which wrote it; Dashboard's two
  // columns are put together there, once the page's script runs.
  var homeStyle = localStorage.getItem('radium-home-style');
  if (homeStyle === 'spotlight' || homeStyle === 'compact' || homeStyle === 'dashboard') {
    document.body.classList.add('home-style-' + homeStyle);
  } else {
    document.body.classList.add('home-style-showcase');
  }
  // Every network now has a rooms/people API, so none start socially hidden.
  // (applyNetworkUI still toggles `network-no-social` from NETWORKS[*].hasSocial
  // if a future network opts out.)
} catch (e) {}
