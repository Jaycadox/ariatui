const STORAGE_KEY = "remotes";
const MENU_ROOT_ID = "download-via-ariatui";
const MENU_PREFIX = "download-via-ariatui:";
const DEFAULT_REMOTE = {
  id: "local",
  label: "Local",
  base_url: "http://127.0.0.1:39123",
  auth_token: null,
  created_at: 0
};
const NOTIFICATION_ICON = "icons/ariatui-96.svg";
const pendingDownloads = new Map();
const AUTH_COOKIE_NAME = "ariatui_auth";

function reportError(error) {
  console.error("AriatUI extension error:", error);
}

function remoteSort(left, right) {
  const leftIsLocal = left.label === "Local";
  const rightIsLocal = right.label === "Local";
  if (leftIsLocal !== rightIsLocal) {
    return leftIsLocal ? -1 : 1;
  }
  return (left.created_at || 0) - (right.created_at || 0);
}

function normalizeBaseUrl(input) {
  const value = String(input || "").trim();
  if (!value) {
    throw new Error("Base URL cannot be empty.");
  }
  const url = new URL(value);
  if (url.protocol !== "http:" && url.protocol !== "https:") {
    throw new Error("Base URL must use http or https.");
  }
  url.pathname = "/";
  url.search = "";
  url.hash = "";
  return url.toString().replace(/\/$/, "");
}

function originPattern(baseUrl) {
  const url = new URL(baseUrl);
  return `${url.origin}/*`;
}

async function notify(title, message) {
  await browser.notifications.create({
    type: "basic",
    title,
    message,
    iconUrl: browser.runtime.getURL(NOTIFICATION_ICON)
  });
}

function tokenExpiryUnix(token) {
  const parts = String(token || "").split(".");
  if (parts.length !== 4) {
    return null;
  }
  const expiry = Number(parts[1]);
  return Number.isFinite(expiry) ? expiry : null;
}

async function syncWebUiCookie(remote) {
  if (!remote.auth_token) {
    return;
  }
  const url = new URL(remote.base_url);
  const details = {
    url: `${url.origin}/`,
    name: AUTH_COOKIE_NAME,
    value: remote.auth_token,
    path: "/",
    secure: url.protocol === "https:",
    httpOnly: true,
    sameSite: "strict"
  };
  const expiryUnix = tokenExpiryUnix(remote.auth_token);
  if (expiryUnix) {
    details.expirationDate = expiryUnix;
  }
  await browser.cookies.set(details);
}

async function getRemotes() {
  const stored = await browser.storage.local.get(STORAGE_KEY);
  const remotes = Array.isArray(stored[STORAGE_KEY]) ? stored[STORAGE_KEY] : [];
  return remotes.slice().sort(remoteSort);
}

async function saveRemotes(remotes) {
  await browser.storage.local.set({
    [STORAGE_KEY]: remotes.slice().sort(remoteSort)
  });
  await rebuildMenus();
}

async function ensureSeededRemotes() {
  const remotes = await getRemotes();
  if (remotes.length > 0) {
    return remotes;
  }
  const seeded = [{ ...DEFAULT_REMOTE }];
  await saveRemotes(seeded);
  return seeded;
}

async function rebuildMenus() {
  await browser.contextMenus.removeAll();
  const remotes = await ensureSeededRemotes();
  browser.contextMenus.create({
    id: MENU_ROOT_ID,
    title: "Download via AriatUI",
    contexts: ["link"]
  });
  for (const remote of remotes) {
    browser.contextMenus.create({
      id: `${MENU_PREFIX}${remote.id}`,
      parentId: MENU_ROOT_ID,
      title: remote.label,
      contexts: ["link"]
    });
  }
}

async function requestRemotePermission(baseUrl) {
  return browser.permissions.request({
    origins: [originPattern(baseUrl)]
  });
}

async function hasRemotePermission(baseUrl) {
  return browser.permissions.contains({
    origins: [originPattern(baseUrl)]
  });
}

async function ensureRemotePermission(baseUrl, interactive) {
  if (await hasRemotePermission(baseUrl)) {
    return true;
  }
  if (!interactive) {
    return false;
  }
  return requestRemotePermission(baseUrl);
}

async function apiFetch(remote, path, options = {}) {
  const headers = new Headers(options.headers || {});
  if (remote.auth_token) {
    headers.set("Authorization", `Bearer ${remote.auth_token}`);
  }
  if (options.json !== undefined) {
    headers.set("Content-Type", "application/json");
  }
  const response = await fetch(`${remote.base_url}${path}`, {
    ...options,
    headers,
    body: options.json === undefined ? options.body : JSON.stringify(options.json)
  });
  return response;
}

async function createPendingDownload(remote, payload) {
  const requestId = crypto.randomUUID();
  pendingDownloads.set(requestId, {
    requestId,
    remoteId: remote.id,
    remoteLabel: remote.label,
    remoteBaseUrl: remote.base_url,
    url: payload.url,
    urlFilename: payload.url_filename,
    remoteFilename: payload.remote_filename,
    remoteLabelForFilename: payload.remote_label,
    finalUrl: payload.final_url || null,
    createdAt: Date.now()
  });
  await browser.windows.create({
    type: "popup",
    width: 920,
    height: 560,
    url: browser.runtime.getURL(`chooser.html?request=${encodeURIComponent(requestId)}`)
  });
}

async function saveRemote(input) {
  const remotes = await ensureSeededRemotes();
  const normalizedBaseUrl = normalizeBaseUrl(input.base_url);
  const trimmedLabel = String(input.label || "").trim();
  if (!trimmedLabel) {
    throw new Error("Label cannot be empty.");
  }

  const existing = remotes.find((remote) => remote.id === input.id) || null;
  const nextRemote = {
    id: existing ? existing.id : crypto.randomUUID(),
    label: trimmedLabel,
    base_url: normalizedBaseUrl,
    auth_token:
      existing && existing.base_url === normalizedBaseUrl ? existing.auth_token : null,
    created_at: existing ? existing.created_at : Date.now()
  };

  const nextRemotes = remotes
    .filter((remote) => remote.id !== nextRemote.id)
    .concat(nextRemote);
  await saveRemotes(nextRemotes);
  return nextRemote;
}

async function deleteRemote(remoteId) {
  const remotes = await ensureSeededRemotes();
  const nextRemotes = remotes.filter((remote) => remote.id !== remoteId);
  await saveRemotes(nextRemotes.length > 0 ? nextRemotes : [{ ...DEFAULT_REMOTE }]);
}

async function forgetRemote(remoteId) {
  const remotes = await ensureSeededRemotes();
  const remote = remotes.find((item) => item.id === remoteId);
  if (!remote) {
    throw new Error("Remote not found.");
  }
  if (remote.auth_token) {
    try {
      await apiFetch(remote, "/api/session", { method: "DELETE" });
    } catch (_) {
    }
  }
  const nextRemote = { ...remote, auth_token: null };
  await saveRemotes(remotes.map((item) => (item.id === remoteId ? nextRemote : item)));
  return nextRemote;
}

async function pairRemote(remoteId) {
  const remotes = await ensureSeededRemotes();
  const remote = remotes.find((item) => item.id === remoteId);
  if (!remote) {
    throw new Error("Remote not found.");
  }
  if (!(await hasRemotePermission(remote.base_url))) {
    throw new Error("Permission to access that AriatUI origin has not been granted.");
  }
  const response = await apiFetch(remote, "/api/pairings", {
    method: "POST",
    json: {}
  });
  if (!response.ok) {
    const body = await response.json().catch(() => ({ error: "Pairing failed." }));
    throw new Error(body.error || "Pairing failed.");
  }
  const body = await response.json();
  return {
    remote,
    request_id: body.request_id,
    pin: body.pin,
    expires_in_secs: body.expires_in_secs
  };
}

async function pollPairing(remoteId, requestId) {
  const remotes = await ensureSeededRemotes();
  const remote = remotes.find((item) => item.id === remoteId);
  if (!remote) {
    throw new Error("Remote not found.");
  }
  const response = await apiFetch(remote, `/api/pairings/${encodeURIComponent(requestId)}`);
  if (!response.ok) {
    const body = await response.json().catch(() => ({ error: "Pairing status failed." }));
    throw new Error(body.error || "Pairing status failed.");
  }
  const body = await response.json();
  if (body.status === "approved") {
    const nextRemote = {
      ...remote,
      auth_token: body.auth_token
    };
    await saveRemotes(remotes.map((item) => (item.id === remoteId ? nextRemote : item)));
  }
  return body;
}

async function testRemote(remoteId) {
  const remotes = await ensureSeededRemotes();
  const remote = remotes.find((item) => item.id === remoteId);
  if (!remote) {
    throw new Error("Remote not found.");
  }
  if (!(await hasRemotePermission(remote.base_url))) {
    throw new Error("Permission to access that AriatUI origin has not been granted.");
  }
  const response = await apiFetch(remote, "/api/session");
  if (response.status === 204) {
    return { status: "paired" };
  }
  if (response.status === 401) {
    if (remote.auth_token) {
      const nextRemote = { ...remote, auth_token: null };
      await saveRemotes(remotes.map((item) => (item.id === remoteId ? nextRemote : item)));
      return { status: "stale_token" };
    }
    return { status: "needs_pairing" };
  }
  const body = await response.json().catch(() => ({ error: "Remote test failed." }));
  throw new Error(body.error || "Remote test failed.");
}

async function openWebUi(remoteId) {
  const remotes = await ensureSeededRemotes();
  const remote = remotes.find((item) => item.id === remoteId);
  if (!remote) {
    throw new Error("Remote not found.");
  }
  if (remote.auth_token) {
    await syncWebUiCookie(remote);
  }
  await browser.tabs.create({ url: remote.base_url });
}

async function getPendingDownload(requestId) {
  const pending = pendingDownloads.get(requestId);
  if (!pending) {
    throw new Error("Pending download request not found.");
  }
  return pending;
}

async function submitPendingDownload(requestId, filename) {
  const pending = await getPendingDownload(requestId);
  const remotes = await ensureSeededRemotes();
  const remote = remotes.find((item) => item.id === pending.remoteId);
  if (!remote) {
    pendingDownloads.delete(requestId);
    throw new Error("Remote not found.");
  }
  if (!remote.auth_token) {
    pendingDownloads.delete(requestId);
    throw new Error(`${remote.label} is not paired anymore.`);
  }

  const response = await apiFetch(remote, "/api/downloads", {
    method: "POST",
    json: { url: pending.url, filename }
  });
  if (response.status === 401) {
    const nextRemote = { ...remote, auth_token: null };
    await saveRemotes(remotes.map((item) => (item.id === remote.id ? nextRemote : item)));
    pendingDownloads.delete(requestId);
    throw new Error(`${remote.label} needs pairing again.`);
  }
  const body = await response.json().catch(() => null);
  if (!response.ok || body?.status !== "queued") {
    throw new Error((body && body.error) || "Download request failed.");
  }
  pendingDownloads.delete(requestId);
  await notify("AriatUI", `Queued on ${remote.label}: ${body.display_name}`);
  return body;
}

async function cancelPendingDownload(requestId) {
  pendingDownloads.delete(requestId);
}

async function downloadViaRemote(remoteId, linkUrl, interactivePermission) {
  const remotes = await ensureSeededRemotes();
  const remote = remotes.find((item) => item.id === remoteId);
  if (!remote) {
    await notify("AriatUI", "Selected remote no longer exists.");
    return;
  }
  if (!linkUrl) {
    await notify("AriatUI", "No link URL was provided by Firefox.");
    return;
  }
  if (!remote.auth_token) {
    await notify("AriatUI", `${remote.label} is not paired yet. Opening settings.`);
    await browser.runtime.openOptionsPage();
    return;
  }
  const permitted = await ensureRemotePermission(remote.base_url, interactivePermission);
  if (!permitted) {
    await notify("AriatUI", `Permission denied for ${remote.label}.`);
    await browser.runtime.openOptionsPage();
    return;
  }

  try {
    const response = await apiFetch(remote, "/api/downloads", {
      method: "POST",
      json: { url: linkUrl, filename: null }
    });
    if (response.status === 401) {
      const nextRemote = { ...remote, auth_token: null };
      await saveRemotes(remotes.map((item) => (item.id === remoteId ? nextRemote : item)));
      await notify("AriatUI", `${remote.label} needs pairing again.`);
      await browser.runtime.openOptionsPage();
      return;
    }
    const body = await response.json().catch(() => null);
    if (response.ok && body?.status === "needs_filename") {
      await createPendingDownload(remote, body);
      return;
    }
    if (!response.ok) {
      throw new Error((body && body.error) || "Download request failed.");
    }
    await notify("AriatUI", `Queued on ${remote.label}: ${body.display_name}`);
  } catch (error) {
    await notify(
      "AriatUI",
      `Could not reach ${remote.label}: ${error instanceof Error ? error.message : String(error)}`
    );
    await browser.runtime.openOptionsPage();
  }
}

browser.runtime.onInstalled.addListener(() => {
  void ensureSeededRemotes().then(rebuildMenus).catch(reportError);
});

browser.runtime.onStartup.addListener(() => {
  void ensureSeededRemotes().then(rebuildMenus).catch(reportError);
});

browser.contextMenus.onClicked.addListener((info) => {
  if (!String(info.menuItemId).startsWith(MENU_PREFIX)) {
    return;
  }
  const remoteId = String(info.menuItemId).slice(MENU_PREFIX.length);
  void downloadViaRemote(remoteId, info.linkUrl, true).catch(reportError);
});

browser.action.onClicked.addListener(() => {
  void browser.runtime.openOptionsPage().catch(reportError);
});

browser.runtime.onMessage.addListener((message) => {
  switch (message.type) {
    case "listRemotes":
      return ensureSeededRemotes();
    case "saveRemote":
      return saveRemote(message.remote);
    case "deleteRemote":
      return deleteRemote(message.remoteId);
    case "forgetRemote":
      return forgetRemote(message.remoteId);
    case "pairRemote":
      return pairRemote(message.remoteId);
    case "pollPairing":
      return pollPairing(message.remoteId, message.requestId);
    case "testRemote":
      return testRemote(message.remoteId);
    case "openWebUi":
      return openWebUi(message.remoteId);
    case "getPendingDownload":
      return getPendingDownload(message.requestId);
    case "submitPendingDownload":
      return submitPendingDownload(message.requestId, message.filename);
    case "cancelPendingDownload":
      return cancelPendingDownload(message.requestId);
    default:
      return undefined;
  }
});

void ensureSeededRemotes().then(rebuildMenus).catch(reportError);
