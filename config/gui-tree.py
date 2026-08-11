#!/usr/bin/python3
"""Print the AT-SPI accessibility tree for applications on the virtual display."""

import argparse
import json
import sys

import pyatspi


def safe(call, default):
    try:
        return call()
    except Exception:
        return default


def node_details(node, path):
    role = safe(node.getRoleName, "unknown")
    name = safe(lambda: node.name or "", "")
    states = safe(
        lambda: [pyatspi.stateToString(state) for state in node.getState().getStates()], []
    )
    actions = safe(
        lambda: [
            node.queryAction().getName(index)
            for index in range(node.queryAction().nActions)
        ],
        [],
    )

    def get_bounds():
        rect = node.queryComponent().getExtents(pyatspi.DESKTOP_COORDS)
        return {"x": rect.x, "y": rect.y, "width": rect.width, "height": rect.height}

    return {
        "path": path,
        "role": role,
        "name": name,
        "states": states,
        "actions": actions,
        "bounds": safe(get_bounds, None),
    }


def walk(node, path, depth, limit, emitted):
    if emitted[0] >= limit:
        return
    details = node_details(node, path)
    emitted[0] += 1
    yield details
    if depth == 0:
        return
    child_count = safe(lambda: node.childCount, 0)
    for index in range(child_count):
        child = safe(lambda index=index: node.getChildAtIndex(index), None)
        if child is not None:
            yield from walk(child, f"{path}/{index}", depth - 1, limit, emitted)


def resolve_path(root, path):
    node = root
    for part in path.split("/"):
        if not part.isdigit():
            raise ValueError(f"invalid node path '{path}'")
        node = node.getChildAtIndex(int(part))
        if node is None:
            raise ValueError(f"node path '{path}' does not exist")
    return node


def invoke_action(desktop, path, requested_action):
    try:
        node = resolve_path(desktop, path)
        action = node.queryAction()
    except Exception as error:
        print(f"gui-tree: cannot resolve actionable node {path}: {error}", file=sys.stderr)
        return 1

    available = [action.getName(index) for index in range(action.nActions)]
    for index, name in enumerate(available):
        if name.casefold() == requested_action.casefold():
            if action.doAction(index):
                print(f"invoked {name} on {path}")
                return 0
            print(f"gui-tree: action '{name}' failed on {path}", file=sys.stderr)
            return 1
    choices = ", ".join(available) if available else "none"
    print(
        f"gui-tree: node {path} has no action '{requested_action}' (available: {choices})",
        file=sys.stderr,
    )
    return 1


def main():
    parser = argparse.ArgumentParser(
        description="Inspect GUI controls through the AT-SPI accessibility tree"
    )
    parser.add_argument(
        "--application", "-a", help="only applications whose name contains this text"
    )
    parser.add_argument("--depth", type=int, default=6)
    parser.add_argument("--limit", type=int, default=500)
    parser.add_argument("--json", action="store_true", help="emit one JSON object per line")
    parser.add_argument(
        "--invoke",
        nargs=2,
        metavar=("PATH", "ACTION"),
        help="invoke a named action on a node path printed by this command",
    )
    args = parser.parse_args()
    if args.depth < 0 or args.limit < 1:
        parser.error("--depth must be non-negative and --limit must be positive")

    desktop = pyatspi.Registry.getDesktop(0)
    if args.invoke:
        return invoke_action(desktop, args.invoke[0], args.invoke[1])

    emitted = [0]
    found_application = False
    for app_index in range(safe(lambda: desktop.childCount, 0)):
        app = safe(lambda app_index=app_index: desktop.getChildAtIndex(app_index), None)
        if app is None:
            continue
        app_name = safe(lambda: app.name or "", "")
        if args.application and args.application.casefold() not in app_name.casefold():
            continue
        found_application = True
        for details in walk(app, str(app_index), args.depth, args.limit, emitted):
            if args.json:
                print(json.dumps(details, ensure_ascii=False))
            else:
                indent = "  " * details["path"].count("/")
                label = details["name"] or "<unnamed>"
                extras = []
                if details["actions"]:
                    extras.append("actions=" + ",".join(details["actions"]))
                if details["bounds"]:
                    bounds = details["bounds"]
                    extras.append(
                        f"bounds={bounds['x']},{bounds['y']} {bounds['width']}x{bounds['height']}"
                    )
                suffix = f" ({'; '.join(extras)})" if extras else ""
                print(f"{indent}{details['path']} {details['role']}: {label}{suffix}")
        if emitted[0] >= args.limit:
            print(f"gui-tree: stopped after {args.limit} nodes", file=sys.stderr)
            break

    if args.application and not found_application:
        print(f"gui-tree: no application matches '{args.application}'", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
