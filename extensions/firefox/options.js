const form = document.getElementById("remote-form");
const resetFormButton = document.getElementById("reset-form");
const cancelEditButton = document.getElementById("cancel-edit");
const statusLine = document.getElementById("status-line");
const remotesRoot = document.getElementById("remotes");
const remoteTemplate = document.getElementById("remote-template");

let remotes = [];

function setStatus(message, isError = false) {
  statusLine.textContent = message;
  statusLine.classList.toggle("error", isError);
}

function formRemote() {
  return {
    id: document.getElementById("remote-id").value || null,
    label: document.getElementById("remote-label").value,
    base_url: document.getElementById("remote-base-url").value
  };
}

function fillForm(remote) {
  document.getElementById("remote-id").value = remote ? remote.id : "";
  document.getElementById("remote-label").value = remote ? remote.label : "Local";
  document.getElementById("remote-base-url").value = remote ? remote.base_url : "http://127.0.0.1:39123";
}

function renderRemotes() {
  remotesRoot.replaceChildren();
  if (remotes.length === 0) {
    remotesRoot.textContent = "No remotes configured.";
    return;
  }

  for (const remote of remotes) {
    const node = remoteTemplate.content.firstElementChild.cloneNode(true);
    node.dataset.remoteId = remote.id;
    node.querySelector(".remote-label").textContent = remote.label;
    node.querySelector(".remote-url").textContent = remote.base_url;
    remotesRoot.append(node);
  }
}

async function refreshRemotes(message = "Remotes ready. Sign into each remote in the web UI before using the right-click menu.") {
  remotes = await browser.runtime.sendMessage({ type: "listRemotes" });
  renderRemotes();
  setStatus(message);
}

async function handleAction(action, remoteId) {
  const remote = remotes.find((item) => item.id === remoteId);
  if (!remote) {
    return;
  }

  switch (action) {
    case "edit":
      fillForm(remote);
      setStatus(`Editing ${remote.label}.`);
      return;
    case "open":
      await browser.runtime.sendMessage({ type: "openWebUi", remoteId });
      setStatus(`Opened ${remote.label}.`);
      return;
    case "delete":
      await browser.runtime.sendMessage({ type: "deleteRemote", remoteId });
      await refreshRemotes(`Deleted ${remote.label}.`);
      return;
    default:
  }
}

form.addEventListener("submit", async (event) => {
  event.preventDefault();
  try {
    const saved = await browser.runtime.sendMessage({
      type: "saveRemote",
      remote: formRemote()
    });
    fillForm(null);
    await refreshRemotes(`Saved ${saved.label}.`);
  } catch (error) {
    setStatus(error.message || String(error), true);
  }
});

resetFormButton.addEventListener("click", () => {
  fillForm(null);
  setStatus("Ready to add a new remote.");
});

cancelEditButton.addEventListener("click", () => {
  fillForm(null);
  setStatus("Edit cancelled.");
});

remotesRoot.addEventListener("click", async (event) => {
  const button = event.target.closest("button[data-action]");
  if (!button) {
    return;
  }
  const remoteId = button.closest(".remote-card")?.dataset.remoteId;
  if (!remoteId) {
    return;
  }
  try {
    await handleAction(button.dataset.action, remoteId);
  } catch (error) {
    setStatus(error.message || String(error), true);
  }
});

fillForm(null);
refreshRemotes().catch((error) => {
  setStatus(error.message || String(error), true);
});
