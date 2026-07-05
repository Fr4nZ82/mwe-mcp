// Progressive enhancement for the token issue form (/dashboard/tokens).
//
// Two jobs, both optional — the form is fully usable without JS:
//   1. Show only the fields that belong to the chosen consumer class
//      (Smart needs an owner; Standard needs an act-as list), and relabel
//      the shared id field ("Device id" vs "Bot id").
//   2. Mirror the id field into the device-label field until the admin
//      types their own label.
//
// No-JS fallback: every field stays visible and the server fills
// device_label from the id when left blank, so nothing here is required
// for correctness.
(function () {
  "use strict";

  var form = document.getElementById("token-issue-form");
  if (!form) return;

  var classRadios = form.querySelectorAll('input[name="consumer_class"]');
  var ownerField = document.getElementById("owner-field");
  var actsAsField = document.getElementById("acts-as-field");
  var idLabel = document.getElementById("consumer-id-label");
  var idHelp = document.getElementById("consumer-id-help");
  var idInput = document.getElementById("consumer_id");
  var deviceLabel = document.getElementById("device_label");

  var COPY = {
    smart: {
      label: "Device id",
      help:
        "Free identifier for this device/session (e.g. cc-laptop). Recorded " +
        "so wiki_admin_op_log can attribute writes to a specific device.",
    },
    standard: {
      label: "Bot id",
      help:
        "The bot's own identity. Issuing creates a credential-less system " +
        "user with this id (and its wiki). Lowercase letters and digits " +
        "only, start with a letter — no hyphen.",
    },
  };

  function currentClass() {
    for (var i = 0; i < classRadios.length; i++) {
      if (classRadios[i].checked) return classRadios[i].value;
    }
    return "smart";
  }

  // Show/hide a branch container and disable its inputs so the hidden
  // branch never submits stray values.
  function setBranch(container, on) {
    if (!container) return;
    container.style.display = on ? "" : "none";
    var inputs = container.querySelectorAll("input, select");
    for (var i = 0; i < inputs.length; i++) inputs[i].disabled = !on;
  }

  function sync() {
    var smart = currentClass() === "smart";
    setBranch(ownerField, smart);
    setBranch(actsAsField, !smart);
    var copy = smart ? COPY.smart : COPY.standard;
    if (idLabel) idLabel.textContent = copy.label;
    if (idHelp) idHelp.textContent = copy.help;
  }

  for (var i = 0; i < classRadios.length; i++) {
    classRadios[i].addEventListener("change", sync);
  }

  // Mirror id -> device label until the admin edits the label themselves.
  if (idInput && deviceLabel) {
    // A sticky re-render may pre-fill a custom label; respect it.
    if (deviceLabel.value && deviceLabel.value !== idInput.value) {
      deviceLabel.dataset.touched = "1";
    }
    idInput.addEventListener("input", function () {
      if (deviceLabel.dataset.touched === "1") return;
      deviceLabel.value = idInput.value;
    });
    deviceLabel.addEventListener("input", function () {
      deviceLabel.dataset.touched =
        deviceLabel.value === idInput.value ? "" : "1";
    });
  }

  sync();
})();
