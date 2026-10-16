"""Native collection folders retain their own identities and membership."""
from collections import deque
from uuid import UUID
from .items import card
from .state import Failure

PATH = "my-collection/playlists/folders"


def folder_id(value):
    value = str(value or "root")
    if value == "root":
        return value
    try:
        return str(UUID(value.removeprefix("folder:")))
    except ValueError:
        raise Failure("Invalid folder reference.", -32602) from None


def collection(catalog):
    pending, seen, folders, playlists = deque(["root"]), set(), [], []
    while pending:
        parent = pending.popleft()
        if parent in seen:
            continue
        seen.add(parent)
        if len(seen) > 1000:
            raise Failure("The folder collection exceeds the current limit.")
        for offset in range(0, 100000, 50):
            page = catalog.get(PATH, {"folderId": parent, "limit": 50, "offset": offset, "order": "NAME"}, True)
            batch = page.get("items", [])
            for entry in batch:
                raw = entry.get("data") or {}
                kind = str(entry.get("itemType", raw.get("itemType", ""))).upper()
                if kind == "FOLDER":
                    key = folder_id(raw.get("id"))
                    if key == "root":
                        continue
                    folders.append({"ref": "folder:" + key, "name": entry.get("name") or raw.get("name", ""), "parent": "" if parent == "root" else "folder:" + parent})
                    pending.append(key)
                elif kind == "PLAYLIST":
                    item = card(raw, "playlist", catalog.client.user)
                    item["native_folder"] = "" if parent == "root" else "folder:" + parent
                    playlists.append(item)
            if len(batch) < 50 or offset + len(batch) >= page.get("totalNumberOfItems", 100001):
                break
        else:
            raise Failure("The playlist collection exceeds the current limit.")
    return {"items": playlists, "folders": folders}


def action(client, params):
    operation = params.get("action")
    if operation == "folder.create":
        name = str(params.get("title", "")).strip()
        if not name:
            raise Failure("Enter a folder name.", -32602)
        client.request(PATH + "/create-folder", method="PUT", v2=True,
                       params={"name": name, "folderId": folder_id(params.get("parent"))})
    elif operation == "folder.rename":
        key = folder_id(params.get("ref"))
        name = str(params.get("title", "")).strip()
        if key == "root" or not name:
            raise Failure("Choose a folder and a name.", -32602)
        client.request(PATH + "/rename", method="PUT", v2=True, params={"trn": "trn:folder:" + key, "name": name})
    elif operation == "playlist.folder":
        from .items import reference
        key = reference(params.get("ref", ""), "playlist")[1]
        client.request(PATH + "/move", method="PUT", v2=True,
                       params={"trns": "trn:playlist:" + key, "folderId": folder_id(params.get("folder"))})
    else:
        raise Failure("Unknown folder action.", -32602)
    return {"changed_refs": [params.get("ref", "")], "folders_changed": True}
