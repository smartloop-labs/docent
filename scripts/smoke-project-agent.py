"""Exercise a cold project-agent launch from a built Docent binary."""

import argparse
from concurrent.futures import ThreadPoolExecutor
import json
import os
from pathlib import Path
import re
import shutil
import signal
import socket
import subprocess
import tempfile
from threading import Barrier
import time
from urllib.error import HTTPError, URLError
from urllib.parse import quote
from urllib.request import ProxyHandler, Request, build_opener


def smoke(binary, root, case, upgrade_version=None):
    home = root / "smartloop"
    env = {key: value for key, value in os.environ.items() if not key.upper().startswith("SLP_")}
    env.update(
        HOME=str(root),
        XDG_CONFIG_HOME=str(root / "config"),
        XDG_CACHE_HOME=str(root / "cache"),
        XDG_DATA_HOME=str(root / "data"),
        SLP_HOME=str(home),
        SLP_API_HOST="127.0.0.1",
        SLP_API_PORT="0",
        SLP_SUPERVISOR="1",
        SLP_API_PRELOAD_MODEL="0",
        NO_PROXY="127.0.0.1,localhost",
        no_proxy="127.0.0.1,localhost",
    )
    handoff = case == "backend handoff"
    serve_args = ["__agent-serve"]
    serve_cwd = root
    replacement_cli = None
    if case == "missing executable":
        env["SLP_EXE"] = str(root / "nonexistent-docent")
    elif case == "warm versus cold":
        agent_exe = root / "agent-executable"
        agent_exe.symlink_to(binary)
        env["SLP_EXE"] = str(agent_exe)
    elif case in ("installer upgrade", "backend handoff"):
        install_dir = root / "bin"
        install_dir.mkdir()
        binary = Path(shutil.copy2(binary, install_dir / "docent"))
        installer_env = dict(
            env,
            SMARTLOOP_CLI_VERSION=upgrade_version,
            SMARTLOOP_CLI_INSTALL_DIR=str(install_dir),
            PATH=f"{install_dir}{os.pathsep}{env.get('PATH', os.defpath)}",
        )
        old_version = subprocess.check_output([str(binary), "--version"], env=env, timeout=10, text=True).strip()
        if old_version.split()[-1] == upgrade_version:
            raise RuntimeError(f"provide an older binary than the requested upgrade: {old_version}")
        if handoff:
            # Pin process discovery to this test's home and port.
            with socket.socket() as listener:
                listener.bind(("127.0.0.1", 0))
                fixed_port = listener.getsockname()[1]
            env["SLP_PORT"] = str(fixed_port)
            env.pop("SMARTLOOP_API_URL", None)
            serve_args.extend(["--port", str(fixed_port), "--cli-version", old_version.split()[-1]])
            home.mkdir()
            serve_cwd = home
    log_path = root / "backend.log"

    with log_path.open("wb") as log:
        server = subprocess.Popen(
            [str(binary), *serve_args],
            cwd=serve_cwd,
            env=env,
            stdin=subprocess.DEVNULL,
            stdout=log,
            stderr=subprocess.STDOUT,
        )

        def request(path, payload=None, timeout=10, *, expected_status=200):
            data = None if payload is None else json.dumps(payload).encode()
            req = Request(base + path, data=data, headers={"Content-Type": "application/json"})
            try:
                with build_opener(ProxyHandler({})).open(req, timeout=timeout) as response:
                    if response.status != expected_status:
                        raise RuntimeError(f"{path}: expected HTTP {expected_status}, got {response.status}")
                    return json.load(response)
            except HTTPError as exc:
                with exc:
                    body = exc.read(4096).decode(errors="replace")
                if exc.code == expected_status:
                    return json.loads(body)
                raise RuntimeError(f"{path}: expected HTTP {expected_status}, got {exc.code}: {body}") from exc

        try:
            deadline = time.monotonic() + 30
            while time.monotonic() < deadline:
                if server.poll() is not None:
                    raise RuntimeError(f"backend exited during startup: {server.returncode}")
                try:
                    port = int((home / "server.port").read_text().strip())
                    if not 0 < port <= 65535:
                        raise ValueError(f"invalid bound port: {port}")
                    base = f"http://127.0.0.1:{port}"
                    request("/health", timeout=2)
                    break
                except (FileNotFoundError, ValueError, URLError, TimeoutError):
                    time.sleep(0.2)
            else:
                raise RuntimeError("backend did not become healthy within 30 seconds")

            def create_project(name):
                project = root / name
                project.mkdir()
                return request("/v1/projects", {"name": name, "cwd": str(project)})["id"]

            # No inference: this read still has to re-exec Docent and reach the child.
            def models(project_id, expected_status=200):
                return request(
                    f"/v1/projects/{quote(project_id, safe='')}/models",
                    timeout=60,
                    expected_status=expected_status,
                )

            def agents():
                status = request("/agents")
                if not status["supervised"]:
                    raise RuntimeError(f"expected a supervisor: {status}")
                return status["agents"]

            def only_live_agent(project_id):
                entries = agents()
                if len(entries) == 1:
                    agent = entries[0]
                    if (
                        agent["project_id"] == project_id
                        and agent["alive"]
                        and agent["pid"] > 0
                        and agent["pid"] != server.pid
                        and agent["port"] > 0
                    ):
                        return agent["pid"]
                raise RuntimeError(f"expected exactly one live child for {project_id}: {entries}")

            def expect_spawn_failure(project_id):
                response = models(project_id, expected_status=503)
                detail = response.get("detail", "")
                if not isinstance(detail, str) or (
                    f"could not spawn an agent for project {project_id}" not in detail
                    or "(os error 2)" not in detail
                ):
                    raise RuntimeError(f"expected a missing-executable spawn error: {response}")
                request("/health")

            project_id = create_project("project-a")
            before = agents()
            if before:
                raise RuntimeError(f"expected no agents before the first project request: {before}")

            if case == "missing executable":
                expect_spawn_failure(project_id)
                if agents():
                    raise RuntimeError("unexpected project agent after failed spawn")

            elif case == "concurrent cold start":
                barrier = Barrier(2, timeout=5)

                def first_request():
                    barrier.wait()
                    return models(project_id)

                with ThreadPoolExecutor(max_workers=2) as pool:
                    pending = [pool.submit(first_request) for _ in range(2)]
                    for result in pending:
                        result.result()
                only_live_agent(project_id)
                starts = (home / "server.log").read_text(errors="replace").count(
                    f"starting project agent for {project_id}"
                )
                if starts != 1:
                    raise RuntimeError(f"expected one launch attempt, logged {starts}")

            elif case in ("installer upgrade", "backend handoff"):
                if handoff and request("/agents")["supervisor"]["pid"] != server.pid:
                    raise RuntimeError("the test port is not served by the original supervisor")
                installer = Path(__file__).resolve().parents[1] / "install.sh"
                print(f"Upgrading {old_version} to {upgrade_version}; old backend PID {server.pid}", flush=True)
                update = subprocess.run(
                    ["/bin/sh", str(installer)],
                    cwd=root,
                    env=installer_env,
                    stdin=subprocess.DEVNULL,
                    stdout=subprocess.PIPE,
                    stderr=subprocess.STDOUT,
                    timeout=180,
                    text=True,
                )
                if update.returncode:
                    raise RuntimeError(f"installer exited {update.returncode}:\n{update.stdout[-4000:]}")
                installed = install_dir / "docent"
                new_version = subprocess.check_output(
                    [str(installed), "--version"], env=env, timeout=10, text=True
                ).strip()
                if new_version.split()[-1] != upgrade_version:
                    raise RuntimeError(f"expected installed version {upgrade_version}, got {new_version}")
                print(f"Installed {new_version} at {installed.resolve()}; probing original port {port}", flush=True)
                if server.poll() is not None:
                    raise RuntimeError(f"old backend exited during upgrade: {server.returncode}")
                if handoff:
                    replacement_cli = installed
                    with (root / "handoff.log").open("wb") as cli_log:
                        subprocess.run(
                            [str(installed), "agent", "start"],
                            cwd=root,
                            env=env,
                            stdin=subprocess.DEVNULL,
                            stdout=cli_log,
                            stderr=subprocess.STDOUT,
                            timeout=45,
                            check=True,
                        )
                    server.wait(timeout=5)
                    pid = int((home / "agent.pid").read_text().strip())
                    status = request("/agents")
                    if pid <= 0 or pid == server.pid or status["supervisor"]["pid"] != pid:
                        raise RuntimeError(f"expected a new supervisor at the test port: {status}")
                    if status["agents"]:
                        raise RuntimeError(f"expected a cold project after handoff: {status}")
                    print(f"Backend handed off from PID {server.pid} to {pid}", flush=True)
                models(project_id)
                child_pid = only_live_agent(project_id)
                if handoff and child_pid == pid:
                    raise RuntimeError("the replacement supervisor did not launch a separate project agent")

            else:
                models(project_id)
                pid = only_live_agent(project_id)
                if case == "warm versus cold":
                    other = create_project("project-b")
                    agent_exe.unlink()  # Only the test-owned launch symlink moves.
                    models(project_id)
                    expect_spawn_failure(other)
                    if only_live_agent(project_id) != pid:
                        raise RuntimeError("warm project stopped using its original agent")
                    agent_exe.symlink_to(binary)
                    models(other)
                    live = {agent["project_id"]: agent["pid"] for agent in agents() if agent["alive"]}
                    if live.get(project_id) != pid or other not in live:
                        raise RuntimeError(f"expected both projects live after restoring launch path: {live}")
                elif case == "replace exited agent":
                    os.kill(pid, signal.SIGTERM)
                    deadline = time.monotonic() + 15
                    while time.monotonic() < deadline:
                        if not any(agent["pid"] == pid and agent["alive"] for agent in agents()):
                            break
                        time.sleep(0.2)
                    else:
                        raise RuntimeError(f"project agent {pid} did not exit")
                    models(project_id)
                    if only_live_agent(project_id) == pid:
                        raise RuntimeError("project did not get a replacement agent")

            print(f"Docent smoke test passed: {case}")
        finally:
            if replacement_cli is not None:
                subprocess.run(
                    [str(replacement_cli), "agent", "stop"],
                    cwd=root,
                    env=env,
                    stdout=subprocess.DEVNULL,
                    stderr=subprocess.DEVNULL,
                    timeout=15,
                    check=True,
                )
            if server.poll() is None:
                try:
                    server.terminate()
                    server.wait(timeout=15)
                except subprocess.TimeoutExpired:
                    server.kill()
                    server.wait()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("binary", type=Path)
    parser.add_argument(
        "--include-missing-executable",
        action="store_true",
        help="also verify the expected 503 when the agent executable is missing",
    )
    parser.add_argument(
        "--include-lifecycle",
        action="store_true",
        help="also test warm versus cold, agent replacement, and concurrent cold requests",
    )
    parser.add_argument(
        "--upgrade-to",
        metavar="VERSION",
        help="also run install.sh in a temporary install while the supplied older backend is live",
    )
    parser.add_argument(
        "--include-handoff",
        action="store_true",
        help="also test the new CLI replacing the running backend (requires --upgrade-to)",
    )
    args = parser.parse_args()
    if args.include_handoff and not args.upgrade_to:
        parser.error("--include-handoff requires --upgrade-to")
    upgrade_version = args.upgrade_to.removeprefix("v") if args.upgrade_to else None
    if upgrade_version is not None and not re.fullmatch(r"\d+\.\d+\.\d+", upgrade_version):
        parser.error("--upgrade-to requires a release version such as 1.0.76")
    try:
        binary = args.binary.resolve(strict=True)
    except OSError as exc:
        parser.error(str(exc))
    cases = ["cold start"]
    if args.include_missing_executable:
        cases.append("missing executable")
    if args.include_lifecycle:
        cases.extend(["warm versus cold", "replace exited agent", "concurrent cold start"])
    if upgrade_version:
        cases.append("installer upgrade")
    if args.include_handoff:
        cases.append("backend handoff")
    for case in cases:
        with tempfile.TemporaryDirectory(prefix="docent-smoke-", dir="/tmp") as tmp:
            root = Path(tmp)
            try:
                smoke(binary, root, case, upgrade_version)
            except (OSError, ValueError, RuntimeError, KeyError, subprocess.SubprocessError) as exc:
                logs = []
                for log_path in (root / "backend.log", root / "handoff.log", root / "smartloop" / "server.log"):
                    if log_path.is_file():
                        logs.append(f"--- {log_path.name} ---\n{log_path.read_text(errors='replace')[-6000:]}")
                diagnostics = "\n".join(logs)
                parser.exit(1, f"Smoke test failed ({case}): {exc}\n{diagnostics}\n")


if __name__ == "__main__":
    main()
