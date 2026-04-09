const form = document.getElementById("remote-form");
const resetFormButton = document.getElementById("reset-form");
const cancelEditButton = document.getElementById("cancel-edit");
const statusLine = document.getElementById("status-line");
const remotesRoot = document.getElementById("remotes");
const remoteTemplate = document.getElementById("remote-template");
const pairingPanel = document.getElementById("pairing-panel");
const pairingRemote = document.getElementById("pairing-remote");
const pairingPin = document.getElementById("pairing-pin");
const pairingCountdown = document.getElementById("pairing-countdown");
const pairingStatus = document.getElementById("pairing-status");

let remotes = [];
let pairingTimer = null;
let pairingDeadline = 0;
let activePairing = null;

function originPattern(baseUrl) {
  return `${new URL(baseUrl).origin}/*`;
}

async function requestPermissionForBaseUrl(baseUrl) {
  return browser.permissions.request({
    origins: [originPattern(baseUrl)]
  });
}

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

function stopPairing() {
  if (pairingTimer) {
    clearInterval(pairingTimer);
    pairingTimer = null;
  }
  activePairing = null;
  pairingPanel.classList.add("hidden");
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
    node.querySelector(".remote-state").textContent = remote.auth_token ? "paired" : "not paired";
    remotesRoot.append(node);
  }
}

async function refreshRemotes(message = "Remotes ready.") {
  remotes = await browser.runtime.sendMessage({ type: "listRemotes" });
  renderRemotes();
  setStatus(message);
}

async function startPairing(remoteId) {
  const remote = remotes.find((item) => item.id === remoteId);
  if (!remote) {
    throw new Error("Remote not found.");
  }
  const permitted = await requestPermissionForBaseUrl(remote.base_url);
  if (!permitted) {
    throw new Error("Permission to access that AriatUI origin was denied.");
  }
  const result = await browser.runtime.sendMessage({
    type: "pairRemote",
    remoteId
  });
  activePairing = {
    remoteId,
    requestId: result.request_id
  };
  pairingDeadline = Date.now() + result.expires_in_secs * 1000;
  pairingRemote.textContent = `${result.remote.label} · ${result.remote.base_url}`;
  pairingPin.textContent = result.pin;
  pairingStatus.textContent = "Waiting for approval…";
  pairingPanel.classList.remove("hidden");

  const tick = async () => {
    const remaining = Math.max(0, Math.ceil((pairingDeadline - Date.now()) / 1000));
    pairingCountdown.textContent = `Expires in ${remaining}s`;
    if (!activePairing) {
      return;
    }
    if (remaining === 0) {
      pairingStatus.textContent = "Pairing expired.";
      stopPairing();
      await refreshRemotes("Pairing expired.");
      return;
    }
    try {
      const status = await browser.runtime.sendMessage({
        type: "pollPairing",
        remoteId,
        requestId: result.request_id
      });
      if (status.status === "approved") {
        pairingStatus.textContent = "Approved.";
        stopPairing();
        await refreshRemotes("Pairing complete.");
        return;
      }
      if (status.status === "expired") {
        pairingStatus.textContent = "Pairing expired.";
        stopPairing();
        await refreshRemotes("Pairing expired.");
      }
    } catch (error) {
      pairingStatus.textContent = error.message || String(error);
    }
  };

  await tick();
  pairingTimer = setInterval(tick, 1200);
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
    case "pair":
      await startPairing(remoteId);
      return;
    case "test": {
      const permitted = await requestPermissionForBaseUrl(remote.base_url);
      if (!permitted) {
        throw new Error("Permission to access that AriatUI origin was denied.");
      }
      const result = await browser.runtime.sendMessage({ type: "testRemote", remoteId });
      const message =
        result.status === "paired"
          ? `${remote.label} is reachable and paired.`
          : result.status === "stale_token"
            ? `${remote.label} is reachable but needs pairing again.`
            : `${remote.label} is reachable and waiting for pairing.`;
      await refreshRemotes(message);
      return;
    }
    case "forget":
      await browser.runtime.sendMessage({ type: "forgetRemote", remoteId });
      await refreshRemotes(`Cleared pairing for ${remote.label}.`);
      return;
    case "open":
      {
        const permitted = await requestPermissionForBaseUrl(remote.base_url);
        if (!permitted) {
          throw new Error("Permission to access that AriatUI origin was denied.");
        }
      }
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
    const draft = formRemote();
    const permitted = await requestPermissionForBaseUrl(draft.base_url);
    if (!permitted) {
      throw new Error("Permission to access that AriatUI origin was denied.");
    }
    const saved = await browser.runtime.sendMessage({
      type: "saveRemote",
      remote: draft
    });
    fillForm(null);
    await refreshRemotes(`Saved ${saved.label}.`);
  } catch (error) {
    setStatus(error.message || String(error), true);
  }
});

resetFormButton.addEventListener("click", () => {
  fillForm(null);
  stopPairing();
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
