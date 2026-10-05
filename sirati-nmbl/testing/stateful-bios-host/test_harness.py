"""Non-VM checks for actual state-slot decoding and regression assertions."""
import copy
import importlib.util
from pathlib import Path
import unittest
spec = importlib.util.spec_from_file_location("stateful_harness", Path(__file__).with_name("harness.py"))
harness = importlib.util.module_from_spec(spec)
spec.loader.exec_module(harness)

class StateProof(unittest.TestCase):
    def test_real_cbor_primitives_and_size(self):
        wire = bytes.fromhex("a2616101616283f4f5f6")
        self.assertEqual(harness.decode_state(wire.ljust(16384, b"\0")), {"a":1,"b":[False,True,None]})
        with self.assertRaises(RuntimeError): harness.decode_state(wire)
        with self.assertRaises(RuntimeError): harness.decode_state(bytes.fromhex("9f").ljust(16384,b"\0"))

    def test_ready_retry_consumption_preserves_failed_history(self):
        ready = {"last_attempted_generation":1,"last_boot_succeeded":False,
                 "rescue_booted_generation":1,"rescue_exit_retry_in_progress":False,
                 "recovery_attempt":2,"known_good_generations":[None]*20}
        consumed = dict(ready, rescue_booted_generation=None, rescue_exit_retry_in_progress=True)
        harness.check_rescue_state(ready)
        harness.check_rescue_state(consumed, ready, ready=False)
        harness.check_rescue_state(ready, ready)
        for field, value in [("last_boot_succeeded",True),("last_attempted_generation",3),
                             ("rescue_booted_generation",1),("rescue_exit_retry_in_progress",False),
                             ("recovery_attempt",0),("known_good_generations",[1]+[None]*19)]:
            changed=copy.deepcopy(consumed); changed[field]=value
            with self.subTest(field=field), self.assertRaises(RuntimeError):
                harness.check_rescue_state(changed, ready, ready=False)

if __name__ == "__main__": unittest.main()
