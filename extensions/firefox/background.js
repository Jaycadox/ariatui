const STORAGE_KEY = "remotes";
const MENU_ROOT_ID = "download-via-ariatui";
const MENU_PREFIX = "download-via-ariatui:";
const DEFAULT_REMOTE = {
  id: "local",
  label: "Local",
  base_url: "http://127.0.0.1:39123",
  created_at: 0
};

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
  url.pathname = "";
  url.search = "";
  url.hash = "";
  return url.toString().replace(/\/$/, "");
}

function extensionLaunchUrl(baseUrl, linkUrl) {
  const url = new URL(`${baseUrl}/extension/add`);
  url.searchParams.set("url", linkUrl);
  return url.toString();
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

async function openWebUi(remoteId) {
  const remotes = await ensureSeededRemotes();
  const remote = remotes.find((item) => item.id === remoteId);
  if (!remote) {
    throw new Error("Remote not found.");
  }
  await browser.tabs.create({ url: remote.base_url });
}

async function launchDownload(remoteId, linkUrl) {
  const remotes = await ensureSeededRemotes();
  const remote = remotes.find((item) => item.id === remoteId);
  if (!remote) {
    throw new Error("Remote not found.");
  }
  if (!linkUrl) {
    throw new Error("No link URL was provided by Firefox.");
  }
  await browser.windows.create({
    type: "popup",
    width: 960,
    height: 620,
    url: extensionLaunchUrl(remote.base_url, linkUrl)
  });
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
  void launchDownload(remoteId, info.linkUrl).catch(reportError);
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
    case "openWebUi":
      return openWebUi(message.remoteId);
    default:
      return undefined;
  }
});

void ensureSeededRemotes().then(rebuildMenus).catch(reportError);
