use super::command;

command!(memory, 630, "memory", icon: Memory, [], "Inspect or maintain Mnemopi memory", [Session, Owner], false, raw("view|stats|diagnose|clear|reset|enqueue|rebuild", ["view", "stats", "diagnose", "clear", "reset", "enqueue", "rebuild"]) => |host, args| host.memory(args));
