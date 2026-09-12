"""Exercise exported bytes, development aliases and moved pkg-config inputs."""

import json
import os
import shlex
import shutil
import subprocess
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

from runtime import digest
from sdk import MODULES, export_sdk


class SdkTests(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory(prefix="voice SDK ")
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)
        self.prefix = self.root / "prefix"
        self.receipts = self.root / "receipts"
        self.output = self.root / "sdk"
        self.target = "aarch64-apple-darwin"
        for name in ("include/glib-2.0", "lib/glib-2.0/include", "lib/pkgconfig"):
            (self.prefix / name).mkdir(parents=True)
        (self.prefix / "include/glib-2.0/glib.h").write_text("/* SDK fixture */")
        (self.prefix / "lib/glib-2.0/include/glibconfig.h").write_text("/* target */")
        self.library = self.prefix / "lib/libfixture.0.dylib"
        self.library.write_bytes(b"receipt-verified fixture bytes")
        for module in (*MODULES, "zlib"):
            (self.prefix / f"lib/pkgconfig/{module}.pc").write_text(
                "prefix=${pcfiledir}/../..\n"
                f"Name: {module}\nDescription: SDK fixture\nVersion: 1.0\n"
                "Cflags: -I${prefix}/include\nLibs: -L${prefix}/lib -lfixture\n"
            )
        audio = self.prefix / "lib/pkgconfig/gstreamer-audio-1.0.pc"
        audio.write_text(
            audio.read_text() + "Requires.private: gstreamer-tag-1.0, zlib\n"
        )
        (self.receipts / "inspection").mkdir(parents=True)
        self.ci = {
            "target": self.target,
            "build_complete": True,
            "inspection_complete": True,
            "manifest_sha256": digest(Path(__file__).with_name("sources.json")),
            "commit": "a" * 40,
        }
        (self.receipts / "ci.json").write_text(json.dumps(self.ci))
        (self.receipts / "inspection/binaries.json").write_text(
            json.dumps(
                [
                    {
                        "path": "lib/libfixture.0.dylib",
                        "target": self.target,
                        "sha256": digest(self.library),
                    }
                ]
            )
        )

    def test_exported_bytes_and_receipt_survive_move_without_original(self):
        export_sdk(self.prefix, self.receipts, self.target, self.output)
        moved = self.root / "moved SDK"
        self.output.rename(moved)
        shutil.rmtree(self.prefix)
        manifest = json.loads((moved / "sdk.json").read_text())
        actual = {
            path.relative_to(moved).as_posix(): digest(path)
            for path in moved.rglob("*")
            if path.is_file() and path.name != "sdk.json"
        }
        self.assertEqual(
            manifest,
            {
                "schemaVersion": 1,
                "target": self.target,
                "sourceCommit": "a" * 40,
                "sourceManifestSha256": self.ci["manifest_sha256"],
                "files": [
                    {"path": name, "sha256": actual[name]} for name in sorted(actual)
                ],
            },
        )
        self.assert_moved_pkg_config(moved)

    def assert_moved_pkg_config(self, moved):
        pkg_config = os.environ.get("VOICE_PKG_CONFIG") or shutil.which("pkg-config")
        if not pkg_config:
            self.skipTest("pkg-config required for the moved development-input probe")
        result = subprocess.check_output(
            [
                pkg_config,
                "--define-prefix",
                "--cflags",
                "--libs",
                "gstreamer-audio-1.0",
                "gio-2.0",
            ],
            env={
                **os.environ,
                "PKG_CONFIG_PATH": "",
                "PKG_CONFIG_LIBDIR": str(moved / "lib/pkgconfig"),
            },
            text=True,
        ).strip()
        self.assertEqual(
            shlex.split(result),
            [f"-I{moved.as_posix()}/include", f"-L{moved.as_posix()}/lib", "-lfixture"],
        )

    def test_cmake_absolute_exec_prefix_survives_move_without_original(self):
        metadata = self.prefix / "lib/pkgconfig/zlib.pc"
        # Archive extraction changes the input path before SDK export. The
        # declared build prefix therefore need not equal self.prefix.
        original = (
            "prefix=/original build/prefix\nexec_prefix=/original build/prefix\n"
            "libdir=${exec_prefix}/lib\nsharedlibdir=${exec_prefix}/lib\n"
            "includedir=${exec_prefix}/include\n\n"
            "Name: zlib\nDescription: SDK fixture\nVersion: 1.3.2\n"
            "Libs: -L${libdir} -L${sharedlibdir} -lfixture\n"
            "Cflags: -I${includedir}\n"
        )
        metadata.write_text(original)
        source_hash = digest(metadata)
        export_sdk(self.prefix, self.receipts, self.target, self.output)
        self.assertEqual(metadata.read_text(), original)
        moved = self.root / "moved SDK"
        self.output.rename(moved)
        shutil.rmtree(self.prefix)
        self.assert_moved_pkg_config(moved)
        exported = moved / "lib/pkgconfig/zlib.pc"
        self.assertEqual(
            exported.read_text(),
            original.replace(
                "prefix=/original build/prefix\n", "prefix=${pcfiledir}/../..\n", 1
            ).replace(
                "exec_prefix=/original build/prefix\n", "exec_prefix=${prefix}\n"
            ),
        )
        record = next(
            item
            for item in json.loads((moved / "sdk.json").read_text())["files"]
            if item["path"] == "lib/pkgconfig/zlib.pc"
        )
        self.assertEqual(
            record,
            {
                "path": "lib/pkgconfig/zlib.pc",
                "sha256": digest(exported),
                "sourceSha256": source_hash,
            },
        )

    def test_absolute_dependency_variables_rebase_by_whole_prefix(self):
        for module, prefix in (
            ("libffi", "/build/prefix"),
            ("libpcre2-8", "C:/build/prefix"),
        ):
            with self.subTest(module=module):
                metadata = self.prefix / f"lib/pkgconfig/{module}.pc"
                metadata.write_text(
                    f"prefix={prefix}\nexec_prefix=${{prefix}}\n"
                    f"includedir={prefix}/include\nlibdir={prefix}/lib\n"
                    f"Name: {module}\nVersion: 1.0\n"
                    "Cflags: -I${includedir}\nLibs: -L${libdir} -lfixture\n"
                )
                export_sdk(self.prefix, self.receipts, self.target, self.output)
                self.assertEqual(
                    (self.output / f"lib/pkgconfig/{module}.pc").read_text(),
                    "prefix=${pcfiledir}/../..\nexec_prefix=${prefix}\n"
                    "includedir=${prefix}/include\nlibdir=${prefix}/lib\n"
                    f"Name: {module}\nVersion: 1.0\n"
                    "Cflags: -I${includedir}\nLibs: -L${libdir} -lfixture\n",
                )
                shutil.rmtree(self.output)

    def test_ambiguous_or_external_dependency_metadata_removes_output(self):
        metadata = self.prefix / "lib/pkgconfig/zlib.pc"
        for invalid in (
            "exec_prefix=/build/prefix\n",
            "prefix=/build/prefix\nprefix=/other\n",
            "prefix=relative/path\n",
            "prefix=/build/prefix\nexec_prefix=/build/prefix-other\n",
            "prefix=/build/prefix\nincludedir=/outside/include\n",
            "prefix=${pcfiledir}/../..\nincludedir=/outside/include\n",
            "prefix=/build/prefix\nCflags: -I/build/prefix/include\n",
        ):
            with self.subTest(metadata=invalid):
                metadata.write_text(invalid)
                with self.assertRaisesRegex(ValueError, "dependency metadata"):
                    export_sdk(self.prefix, self.receipts, self.target, self.output)
                self.assertFalse(self.output.exists())

    def test_native_mutation_and_wrong_target_are_rejected(self):
        with self.assertRaisesRegex(ValueError, "sources and target"):
            export_sdk(self.prefix, self.receipts, "x86_64-apple-darwin", self.output)
        self.library.write_bytes(b"changed")
        with self.assertRaisesRegex(ValueError, "does not match"):
            export_sdk(self.prefix, self.receipts, self.target, self.output)
        self.assertFalse(self.output.exists())

    def test_development_alias_is_materialized_and_escape_is_rejected(self):
        alias = self.prefix / "lib/libfixture.dylib"
        try:
            alias.symlink_to(self.library.name)
        except OSError:
            self.skipTest("host cannot create the SDK's development symlink")
        export_sdk(self.prefix, self.receipts, self.target, self.output)
        self.assertFalse((self.output / "lib/libfixture.dylib").is_symlink())
        self.assertEqual(
            (self.output / "lib/libfixture.dylib").read_bytes(),
            self.library.read_bytes(),
        )
        shutil.rmtree(self.output)
        alias.unlink()
        alias.symlink_to(self.receipts / "ci.json")
        with self.assertRaisesRegex(ValueError, "inside the prefix"):
            export_sdk(self.prefix, self.receipts, self.target, self.output)

    def test_nonrelocatable_metadata_and_failed_copy_leave_no_output(self):
        metadata = self.prefix / "lib/pkgconfig/gstreamer-1.0.pc"
        original = metadata.read_text()
        metadata.write_text(original.replace("${pcfiledir}/../..", "/old/build"))
        with self.assertRaisesRegex(ValueError, "rebuild the SDK"):
            export_sdk(self.prefix, self.receipts, self.target, self.output)
        metadata.write_text(original)
        with (
            patch("sdk.shutil.copy2", side_effect=OSError("copy failed")),
            self.assertRaises(OSError),
        ):
            export_sdk(self.prefix, self.receipts, self.target, self.output)
        self.assertFalse(self.output.exists())

    def test_output_cannot_overwrite_or_nest_inside_inputs(self):
        for output in (self.prefix, self.prefix / "sdk", self.receipts / "sdk"):
            with (
                self.subTest(output=output),
                self.assertRaisesRegex(ValueError, "fresh"),
            ):
                export_sdk(self.prefix, self.receipts, self.target, output)
