#!/usr/bin/env python3

import argparse
import os
import json
import subprocess
import shutil

ap = argparse.ArgumentParser()
ap.add_argument("--no-split", action="store_true")
ap.add_argument("--optimize", action="store_true")
args = ap.parse_args()

root_dir = os.path.dirname(__file__)

build_options = [
    "--target",
    "wasm32-unknown-unknown",
    "--release",
]

if not args.no_split:
    build_options.extend(["--features", "split"])

# First build without output redirection in order to allow errors to be
# displayed normally
subprocess.run(
    [
        "cargo",
        "build",
    ]
    + build_options,
    cwd=os.path.join(root_dir, "crates", "example"),
    check=True,
)


json_results = subprocess.run(
    ["cargo", "build"]
    + build_options
    + [
        "--message-format=json",
    ],
    stdout=subprocess.PIPE,
    cwd=os.path.join(root_dir, "crates", "example"),
    check=True,
).stdout.splitlines()

target_path = None
for json_result in json_results:
    msg = json.loads(json_result)
    filenames = msg.get("filenames")
    if filenames:
        target_path = filenames[0]

print(target_path)
assert target_path is not None

pkg_dir = os.path.join(root_dir, "pkg")

shutil.rmtree(pkg_dir, ignore_errors=True)

if args.no_split:
    subprocess.run(
        [
            "wasm-bindgen",
            target_path,
            "--out-dir",
            pkg_dir,
            "--out-name",
            "main",
            "--no-demangle",
            "--target",
            "web",
        ],
        cwd=root_dir,
        check=True,
    )
else:
    split_temp_dir = os.path.join(root_dir, "split_tmp")
    shutil.rmtree(split_temp_dir, ignore_errors=True)

    subprocess.run(
        [
            "cargo",
            "run",
            "-p",
            "wasm_split_cli",
            "--",
            target_path,
            split_temp_dir,
        ],
        cwd=root_dir,
        check=True,
    )

    subprocess.run(
        [
            "wasm-bindgen",
            os.path.join(split_temp_dir, "main.wasm"),
            "--out-dir",
            pkg_dir,
            "--no-demangle",
            "--target",
            "web",
            "--keep-lld-exports",
        ],
        cwd=root_dir,
        check=True,
    )

    for name in os.listdir(split_temp_dir):
        if name == "main.wasm":
            continue
        shutil.copyfile(os.path.join(split_temp_dir, name), os.path.join(pkg_dir, name))

    if args.optimize:
        for name in os.listdir(pkg_dir):
            if not name.endswith(".wasm"):
                continue
            path = os.path.join(pkg_dir, name)
            orig_size = os.stat(path).st_size
            subprocess.run(["wasm-opt", "-Os", path, "-o", path], check=True)
            new_size = os.stat(path).st_size
            print(f"wasm-opt: {path}: {orig_size} -> {new_size}")
