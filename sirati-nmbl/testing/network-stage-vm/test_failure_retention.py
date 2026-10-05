"""Host-only proof that failed runtime retention drains without terminating."""
import importlib.util
import io
from pathlib import Path
import subprocess
import sys
import threading
import time
import unittest

spec = importlib.util.spec_from_file_location("rescue_harness", Path(__file__).with_name("harness.py"))
harness = importlib.util.module_from_spec(spec)
spec.loader.exec_module(harness)

class FailureRetention(unittest.TestCase):
    def test_live_runtime_is_retained_and_serial_cannot_block(self):
        child = subprocess.Popen([sys.executable, "-c",
            "import sys,time; sys.stdout.buffer.write(b'x'*131072); sys.stdout.flush(); time.sleep(30)"],
            stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
        transcript = io.BytesIO()
        worker = threading.Thread(target=harness.hold_failed_vm, args=(child, transcript))
        worker.start()
        try:
            deadline = time.monotonic() + 10
            while transcript.tell() < 131072 and time.monotonic() < deadline:
                time.sleep(.02)
            self.assertEqual(transcript.tell(), 131072)
            self.assertIsNone(child.poll())
            self.assertTrue(worker.is_alive())
        finally:
            child.terminate()
            child.wait(timeout=10)
            worker.join(timeout=5)
            child.stdout.close()
        self.assertFalse(worker.is_alive())

if __name__ == "__main__": unittest.main()
