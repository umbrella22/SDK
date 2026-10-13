#!/usr/bin/env python3
"""Direct regression checks for generated C semantics and the version gate.

Run with: python3 -B scripts/test-header.py
Requires C11 and C++11 compilers (CC/CXX, defaulting to cc/c++).
"""

import os
from pathlib import Path
import shlex
import subprocess
import tempfile
import unittest


ROOT = Path(__file__).resolve().parents[1]
HEADER = ROOT / "include" / "uda.h"
CAPABILITIES = (
    ("MANAGEMENT", 16),
    ("LOCK", 17),
    ("LOGOUT", 18),
    ("SUSPEND", 19),
    ("HIBERNATE", 20),
    ("REBOOT", 21),
    ("SHUTDOWN", 22),
)


class HeaderRegressionTests(unittest.TestCase):
    def test_capability_macros_preserve_unsigned_c_and_cpp_semantics(self):
        for language, variable, default, standard in (
            ("c", "CC", "cc", "c11"),
            ("c++", "CXX", "c++", "c++11"),
        ):
            with self.subTest(language=language):
                lines = ['#include "uda.h"']
                if language == "c":
                    assertion = "_Static_assert"
                    unsigned_type = "_Generic(({macro}), unsigned int: 1, default: 0)"
                else:
                    lines.append("#include <type_traits>")
                    assertion = "static_assert"
                    unsigned_type = "std::is_same<decltype({macro}), unsigned int>::value"
                for name, bit in CAPABILITIES:
                    macro = f"UDA_SESSION_CAP_{name}"
                    complement = 0xFFFFFFFF ^ (1 << bit)
                    lines.extend(
                        (
                            f'{assertion}({unsigned_type.format(macro=macro)}, "{macro} type");',
                            f'{assertion}({macro} == {1 << bit}u, "{macro} value");',
                            f'{assertion}((uint64_t)(~{macro}) == UINT64_C({complement}), "{macro} complement");',
                        )
                    )
                compiler = shlex.split(os.environ.get(variable, default))
                result = subprocess.run(
                    compiler
                    + [f"-std={standard}", "-Werror", "-fsyntax-only", "-x", language,
                       "-I", str(HEADER.parent), "-"],
                    input="\n".join(lines) + "\n",
                    text=True,
                    capture_output=True,
                )
                self.assertEqual(result.returncode, 0, result.stdout + result.stderr)

    def test_status_alias_stays_rust_only(self):
        self.assertNotRegex(HEADER.read_text(), r"\bUdaStatus\b")

    def test_wrong_generator_version_exits_before_generation(self):
        scratch = ROOT / ".tmp"
        scratch.mkdir(exist_ok=True)
        header_before = HEADER.read_bytes()
        with tempfile.TemporaryDirectory(prefix="header-test-", dir=scratch) as directory:
            stub_dir = Path(directory)
            calls = stub_dir / "calls"
            stub = stub_dir / "cbindgen"
            stub.write_text(
                '#!/bin/sh\n'
                'printf "%s\\n" "$*" >> "$CBINDGEN_TEST_CALLS"\n'
                'if [ "$#" -eq 1 ] && [ "$1" = "--version" ]; then\n'
                '    printf "cbindgen 0.29.3\\n"\n'
                '    exit 0\n'
                'fi\n'
                'printf "generation reached\\n" >&2\n'
                'exit 99\n'
            )
            stub.chmod(0o755)
            env = os.environ.copy()
            env["PATH"] = str(stub_dir) + os.pathsep + env.get("PATH", "")
            env["CBINDGEN_TEST_CALLS"] = str(calls)
            for arguments in ([], ["--check"]):
                with self.subTest(arguments=arguments):
                    calls.write_text("")
                    result = subprocess.run(
                        ["bash", str(ROOT / "scripts" / "gen-header.sh"), *arguments],
                        env=env,
                        text=True,
                        capture_output=True,
                    )
                    self.assertEqual(result.returncode, 1, result.stdout + result.stderr)
                    self.assertIn("error: cbindgen v0.29.3 installed", result.stderr)
                    self.assertIn("requires v0.29.4", result.stderr)
                    self.assertEqual(calls.read_text(), "--version\n")
                    self.assertEqual(HEADER.read_bytes(), header_before)


if __name__ == "__main__":
    unittest.main()
