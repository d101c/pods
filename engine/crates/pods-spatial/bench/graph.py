"""Queries on a pw-dump of the PRIVATE bench daemon (read from stdin).

  graph.py default            -> "<configured> <effective>" default sink names
  graph.py node-id NAME       -> id of the node NAME (empty if none)
  graph.py has-node NAME      -> exit status 0 if the node exists
  graph.py links-from NAME    -> names of the nodes NAME's outputs are linked to
"""
import json
import re
import sys

objs = json.load(sys.stdin)
cmd = sys.argv[1]
nodes = {o["id"]: o["info"]["props"].get("node.name", "") for o in objs
         if o.get("type") == "PipeWire:Interface:Node"}


def default(key):
    for o in objs:
        if o.get("type") == "PipeWire:Interface:Metadata" and o.get("props", {}).get("metadata.name") == "default":
            for m in o.get("metadata") or []:
                if m.get("subject") == 0 and m.get("key") == key:
                    v = m.get("value")
                    if isinstance(v, dict):
                        return v.get("name", "")
                    mm = re.search(r'"name"\s*:\s*"([^"]*)"', str(v))
                    return mm.group(1) if mm else str(v)
    return "-"


if cmd == "default":
    print(default("default.configured.audio.sink"), default("default.audio.sink"))
elif cmd == "node-id":
    print(next((i for i, n in nodes.items() if n == sys.argv[2]), ""))
elif cmd == "has-node":
    sys.exit(0 if sys.argv[2] in nodes.values() else 1)
elif cmd == "links-from":
    src = [i for i, n in nodes.items() if n == sys.argv[2]]
    out = sorted({nodes.get(o["info"]["input-node-id"], "?") for o in objs
                  if o.get("type") == "PipeWire:Interface:Link" and o["info"]["output-node-id"] in src})
    print(" ".join(out))
elif cmd == "states":
    for o in objs:
        if o.get("type") == "PipeWire:Interface:Node":
            p = o["info"]["props"]
            print("node %-28s %-10s driver=%s passive=%s always=%s" % (
                p.get("node.name"), o["info"].get("state"), p.get("node.driver"),
                p.get("node.passive"), p.get("node.always-process")))
    for o in objs:
        if o.get("type") == "PipeWire:Interface:Link":
            i = o["info"]
            print("link %s -> %s state=%s passive=%s" % (
                nodes.get(i["output-node-id"]), nodes.get(i["input-node-id"]), i.get("state"),
                (i.get("props") or {}).get("link.passive")))
else:
    sys.exit("unknown query " + cmd)
