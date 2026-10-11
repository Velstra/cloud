"""Role and agent trust reconciliation during control-plane deployment."""
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


class AgentTrust(unittest.TestCase):
    def test_trust_path_changes_even_when_endpoint_stays_the_same(self):
        lines = ['VELSTRA_NODE=node-a', 'VELSTRA_API_URL=https://api.example',
                 'VELSTRA_API_CA=/old/api.pem']
        updated = registration.agent_seed(lines, 'https://api.example', '/new/ca.pem')
        self.assertIn('VELSTRA_NODE=node-a', updated)
        self.assertIn('VELSTRA_API_CA=/new/ca.pem', updated)
        self.assertNotIn('VELSTRA_API_CA=/old/api.pem', updated)
        self.assertEqual(lines[-1], 'VELSTRA_API_CA=/old/api.pem')

    def test_duplicate_endpoints_and_trust_paths_are_removed(self):
        lines = ['VELSTRA_API_URL=https://old.example', 'VELSTRA_API_URL=https://api.example',
                 'VELSTRA_API_CA=/ca.pem', 'VELSTRA_API_CA=/old.pem']
        self.assertEqual(registration.agent_seed(lines, 'https://api.example', '/ca.pem'),
                         ['VELSTRA_API_URL=https://api.example', 'VELSTRA_API_CA=/ca.pem'])

    def test_unchanged_trust_and_endpoint_are_idempotent(self):
        lines = ['# machine identity', 'VELSTRA_NODE=node-a',
                 'VELSTRA_API_URL=https://api.example', 'VELSTRA_API_CA=/ca.pem']
        self.assertEqual(registration.agent_seed(lines, 'https://api.example', '/ca.pem'), lines)


if __name__ == '__main__':
    unittest.main()
