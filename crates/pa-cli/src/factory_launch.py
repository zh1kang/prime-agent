"""Choose an installed machine and prepare its run call for the chat composer."""
import json
import os
from pathlib import Path
import sys
import tomllib


def choose():
    machines = machine_catalog(sys.argv[1])
    names = list(machines)
    if not names:
        raise ValueError("No machines installed")
    for number, name in enumerate(names, 1):
        print(f"{number:2}  {name}")
    while True:
        choice = input("Select machine (Enter to cancel)> ").strip()
        if not choice:
            return
        if choice.isdecimal() and 1 <= int(choice) <= len(names):
            choice = names[int(choice) - 1]
        if choice in machines:
            break
        print("Choose a listed name or number.")
    params = {}
    path = Path(machines[choice]["path"])
    if path.name == "machine.toml":
        with path.open("rb") as stream:
            fields = tomllib.load(stream).get("params", {})
        for field, spec in fields.items():
            while True:
                hint = " (Enter for default)" if "default" in spec else ""
                value = input(f"{field}{hint}> ").strip()
                if not value and "default" in spec:
                    break
                if not value:
                    print("This parameter requires a value.")
                    continue
                try:
                    params[field] = value if spec["type"] == "text" else json.loads(value)
                except ValueError:
                    print("Enter a JSON value.")
                    continue
                break
    args = f", params={params!r}" if params else ""
    prompt = f"Start the {choice} factory in this project. In the Python kernel, call await rlm.factory.run({choice!r}{args}) once and report the run ID."
    Path(os.environ["PA_FACTORY_DRAFT"]).write_text(prompt)


try:
    choose()
except (EOFError, KeyboardInterrupt):
    pass
