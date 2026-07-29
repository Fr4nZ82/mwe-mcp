// Frozen-instance chrome: show every control, let none of them fire.
//
// On a shown deployment (`instance.read_only`) the operator consoles are
// mounted like anywhere else, because an instance that hides them is not
// showing the product. Their forms would then look live and answer 403
// on submit — a stranger filling in a form to be told "no" learns less
// than one who can see, at a glance, that this instance is a display
// model.
//
// So every write control is rendered and then made visibly inert here.
// This is chrome, NOT the boundary: the boundary is `read_only::guard`,
// server-side, refusing by path whatever the browser does. A visitor
// with the developer console open can re-enable any of these and still
// be refused. That is the intended order — shut the door, then take the
// handle off the inside.
//
// The exempt list is rendered into the page from `ALLOWED_WRITES` (see
// `ui::layout`), so the two cannot drift: whatever the server still
// accepts is exactly what stays clickable here.
(function () {
  'use strict';

  var LIVE = window.__mweLiveWrites || [];
  var REASON =
    'This instance is read-only: it is a live demonstration, so nothing here can be changed.';

  function isLive(form) {
    var action = form.getAttribute('action') || '';
    // Compare on the path only — an action may be absolute, relative, or
    // carry a query string.
    var path = action;
    var scheme = path.indexOf('://');
    if (scheme !== -1) {
      var slash = path.indexOf('/', scheme + 3);
      path = slash === -1 ? '' : path.slice(slash);
    }
    path = path.split('?')[0].split('#')[0];
    // An empty action posts back to the current URL.
    if (path === '') path = window.location.pathname;
    for (var i = 0; i < LIVE.length; i++) {
      if (path === LIVE[i]) return true;
    }
    return false;
  }

  function freeze(root) {
    var forms = root.querySelectorAll('form');
    for (var i = 0; i < forms.length; i++) {
      var form = forms[i];
      if (isLive(form)) continue;
      form.setAttribute('data-read-only', '1');
      form.addEventListener('submit', function (e) {
        e.preventDefault();
      });
      var controls = form.querySelectorAll('input, select, textarea, button');
      for (var j = 0; j < controls.length; j++) {
        var c = controls[j];
        // A hidden input carries no affordance; disabling it only makes
        // the markup noisier.
        if (c.type === 'hidden') continue;
        c.disabled = true;
        c.setAttribute('aria-disabled', 'true');
        if (!c.title) c.title = REASON;
        c.style.opacity = '0.55';
        c.style.cursor = 'not-allowed';
      }
    }
  }

  if (document.readyState === 'loading') {
    document.addEventListener('DOMContentLoaded', function () {
      freeze(document);
    });
  } else {
    freeze(document);
  }
})();
