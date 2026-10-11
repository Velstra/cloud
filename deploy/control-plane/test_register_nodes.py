"""Role preservation when a machine joins the control plane."""
import importlib.util
from pathlib import Path
import unittest

module = importlib.util.spec_from_file_location(
    'register_nodes', Path(__file__).with_name('register-nodes.py'))
registration = importlib.util.module_from_spec(module)
module.loader.exec_module(registration)


class ControlPlaneRoles(unittest.TestCase):
    def test_legacy_compute_semantics_survive_registration(self):
        for spec in ({}, {'roles': []}):
            with self.subTest(spec=spec):
                self.assertEqual(registration.control_plane_roles(spec),
                                 ['compute', 'control-plane'])

    def test_explicit_services_are_preserved(self):
        spec = {'roles': ['compute', 'storage']}
        self.assertEqual(registration.control_plane_roles(spec),
                         ['compute', 'storage', 'control-plane'])
        self.assertEqual(spec['roles'], ['compute', 'storage'])

    def test_control_only_nodes_stay_control_only_on_repeated_runs(self):
        spec = {'roles': ['control-plane']}
        self.assertEqual(registration.control_plane_roles(spec), ['control-plane'])


if __name__ == '__main__':
    unittest.main()
