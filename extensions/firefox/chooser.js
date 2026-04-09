const params = new URLSearchParams(window.location.search);
const requestId = params.get("request");
const form = document.getElementById("chooser-form");
const statusLine = document.getElementById("status");
const customInput = document.getElementById("custom-filename");
let pending = null;

function setStatus(message, isError = false) {
  statusLine.textContent = message;
  statusLine.classList.toggle("error", isError);
}

function selectedChoice() {
  return document.querySelector('input[name="filename_choice"]:checked')?.value || "url";
}

function resolvedFilename() {
  const choice = selectedChoice();
  if (choice === "remote") {
    return pending.remoteFilename;
  }
  if (choice === "custom") {
    return customInput.value.trim();
  }
  return pending.urlFilename;
}

function syncCustomState() {
  customInput.disabled = selectedChoice() !== "custom";
}

async function loadPending() {
  if (!requestId) {
    throw new Error("Missing chooser request id.");
  }
  pending = await browser.runtime.sendMessage({
    type: "getPendingDownload",
    requestId
  });
  document.getElementById("remote-line").textContent =
    `${pending.remoteLabel} · ${pending.remoteBaseUrl}`;
  document.getElementById("url-line").textContent = pending.finalUrl || pending.url;
  document.getElementById("url-filename").textContent = pending.urlFilename;
  document.getElementById("remote-label-title").textContent =
    `Use ${pending.remoteLabelForFilename}`;
  document.getElementById("remote-filename").textContent = pending.remoteFilename;
  customInput.value = pending.remoteFilename;
  syncCustomState();
}

document.querySelectorAll('input[name="filename_choice"]').forEach((input) => {
  input.addEventListener("change", syncCustomState);
});

form.addEventListener("submit", async (event) => {
  event.preventDefault();
  const filename = resolvedFilename();
  if (!filename) {
    setStatus("Filename cannot be empty.", true);
    return;
  }
  try {
    setStatus("Submitting…");
    await browser.runtime.sendMessage({
      type: "submitPendingDownload",
      requestId,
      filename
    });
    window.close();
  } catch (error) {
    setStatus(error.message || String(error), true);
  }
});

document.getElementById("cancel").addEventListener("click", async () => {
  await browser.runtime.sendMessage({
    type: "cancelPendingDownload",
    requestId
  });
  window.close();
});

loadPending().catch((error) => {
  setStatus(error.message || String(error), true);
  form.querySelector('button[type="submit"]').disabled = true;
});
