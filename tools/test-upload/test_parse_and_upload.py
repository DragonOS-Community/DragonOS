#!/usr/bin/env python3
"""
单元测试：测试日志解析功能
"""

import unittest
from parse_and_upload import GoogleTestParser, GenericParser


class TestGoogleTestParser(unittest.TestCase):
    """Google Test解析器测试"""
    
    def setUp(self):
        self.parser = GoogleTestParser()
    
    def test_simple_pass(self):
        """测试简单的通过用例"""
        log = """[ RUN      ] TestName
[       OK ] TestName (4 ms)"""
        result = self.parser.parse(log)
        self.assertEqual(len(result), 1)
        self.assertEqual(result[0]['name'], 'TestName')
        self.assertEqual(result[0]['status'], 'passed')
        self.assertEqual(result[0]['duration_ms'], 4)
    
    def test_simple_fail(self):
        """测试简单的失败用例（场景2）"""
        log = """[ RUN      ] TestName
test/syscalls/linux/test.cc:42: Failure
Value of: x
Expected: is equal to 1
  Actual: 2
[  FAILED  ] TestName (4 ms)"""
        result = self.parser.parse(log)
        self.assertEqual(len(result), 1)
        self.assertEqual(result[0]['name'], 'TestName')
        self.assertEqual(result[0]['status'], 'failed')
        self.assertEqual(result[0]['duration_ms'], 4)
        self.assertIn('error_log', result[0])
        self.assertIn('test/syscalls/linux/test.cc:42', result[0]['error_log'])
    
    def test_fail_without_failed_marker(self):
        """测试失败场景1：只有RUN没有结果但有错误信息"""
        log = """[ RUN      ] TestName
test/syscalls/linux/test.cc:425: Failure
Value of: futex_wake_op(IsPrivate(), &a, &b, 1, 1, (((1 & 0xf) << 28) | ((0 & 0xf) << 24) | ((1 & 0xfff) << 12) | ((kInitialValue + 1) & 0xfff)))
Expected: is equal to 2
  Actual: 1 (of type int)
[ RUN      ] NextTest"""
        result = self.parser.parse(log)
        self.assertEqual(len(result), 2)
        self.assertEqual(result[0]['name'], 'TestName')
        self.assertEqual(result[0]['status'], 'failed')
        self.assertIn('error_log', result[0])
        self.assertIn('test/syscalls/linux/test.cc:425', result[0]['error_log'])
    
    def test_multiple_tests(self):
        """测试多个测试用例"""
        log = """[ RUN      ] Test1
[       OK ] Test1 (4 ms)
[ RUN      ] Test2
[       OK ] Test2 (8 ms)
[ RUN      ] Test3
[  FAILED  ] Test3 (12 ms)"""
        result = self.parser.parse(log)
        self.assertEqual(len(result), 3)
        self.assertEqual(result[0]['name'], 'Test1')
        self.assertEqual(result[0]['status'], 'passed')
        self.assertEqual(result[1]['name'], 'Test2')
        self.assertEqual(result[1]['status'], 'passed')
        self.assertEqual(result[2]['name'], 'Test3')
        self.assertEqual(result[2]['status'], 'failed')
    
    def test_run_not_at_line_start(self):
        """测试[ RUN ]不在行首的情况"""
        log = """[DEBUG] some debug info
[ RUN      ] TestName
[       OK ] TestName (4 ms)"""
        result = self.parser.parse(log)
        self.assertEqual(len(result), 1)
        self.assertEqual(result[0]['name'], 'TestName')
        self.assertEqual(result[0]['status'], 'passed')
    
    def test_failed_not_at_line_start(self):
        """测试[ FAILED ]不在行首的情况"""
        log = """[ RUN      ] TestName
[DEBUG] some debug info
[  FAILED  ] TestName (4 ms)"""
        result = self.parser.parse(log)
        self.assertEqual(len(result), 1)
        self.assertEqual(result[0]['name'], 'TestName')
        self.assertEqual(result[0]['status'], 'failed')
    
    def test_complex_test_name(self):
        """测试复杂的测试用例名称（包含斜杠、点等）"""
        log = """[ RUN      ] Waiters/WaitAnyChildTest.Fork/0
[       OK ] Waiters/WaitAnyChildTest.Fork/0 (4 ms)"""
        result = self.parser.parse(log)
        self.assertEqual(len(result), 1)
        self.assertEqual(result[0]['name'], 'Waiters/WaitAnyChildTest.Fork/0')
        self.assertEqual(result[0]['status'], 'passed')
    
    def test_error_log_extraction(self):
        """测试错误日志提取"""
        log = """[ RUN      ] LseekTest.SeekDataAndSeekHole
test/syscalls/linux/lseek.cc:208: Failure
Value of: lseek(fd.get(), mid, 3)
Expected: is equal to 4
  Actual: 8 (of type long)
[  FAILED  ] LseekTest.SeekDataAndSeekHole (4 ms)"""
        result = self.parser.parse(log)
        self.assertEqual(len(result), 1)
        self.assertEqual(result[0]['status'], 'failed')
        self.assertIn('error_log', result[0])
        error_log = result[0]['error_log']
        self.assertIn('test/syscalls/linux/lseek.cc:208', error_log)
        self.assertIn('Expected: is equal to 4', error_log)
        self.assertIn('Actual: 8', error_log)
    
    def test_futx_example(self):
        """测试用户提供的futex示例"""
        log = """[ RUN      ] SharedPrivate/PrivateAndSharedFutexTest.WakeOpCondSuccess/0
test/syscalls/linux/futex.cc:425: Failure
Value of: futex_wake_op(IsPrivate(), &a, &b, 1, 1, (((1 & 0xf) << 28) | ((0 & 0xf) << 24) | ((1 & 0xfff) << 12) | ((kInitialValue + 1) & 0xfff)))
Expected: is equal to 2
  Actual: 1 (of type int)"""
        result = self.parser.parse(log)
        self.assertEqual(len(result), 1)
        self.assertEqual(result[0]['name'], 'SharedPrivate/PrivateAndSharedFutexTest.WakeOpCondSuccess/0')
        self.assertEqual(result[0]['status'], 'failed')
        self.assertIn('error_log', result[0])
        error_log = result[0]['error_log']
        self.assertIn('test/syscalls/linux/futex.cc:425', error_log)
        self.assertIn('Expected: is equal to 2', error_log)
    
    def test_real_world_example(self):
        """测试真实世界的例子（从serial_opt.txt）"""
        log = """[ RUN      ] Waiters/WaitAnyChildTest.Fork/0
[       OK ] Waiters/WaitAnyChildTest.Fork/0 (4 ms)
[ RUN      ] Waiters/WaitAnyChildTest.Fork/1
[       OK ] Waiters/WaitAnyChildTest.Fork/1 (4 ms)"""
        result = self.parser.parse(log)
        self.assertEqual(len(result), 2)
        self.assertEqual(result[0]['name'], 'Waiters/WaitAnyChildTest.Fork/0')
        self.assertEqual(result[0]['status'], 'passed')
        self.assertEqual(result[0]['duration_ms'], 4)
        self.assertEqual(result[1]['name'], 'Waiters/WaitAnyChildTest.Fork/1')
        self.assertEqual(result[1]['status'], 'passed')
        self.assertEqual(result[1]['duration_ms'], 4)


class TestGenericParser(unittest.TestCase):
    """通用解析器测试"""
    
    def setUp(self):
        self.parser = GenericParser()
    
    def test_google_test_format(self):
        """测试Google Test格式"""
        log = """[ RUN      ] TestName
[       OK ] TestName (4 ms)"""
        result = self.parser.parse(log)
        self.assertEqual(len(result), 1)
        self.assertEqual(result[0]['name'], 'TestName')
        self.assertEqual(result[0]['status'], 'passed')


if __name__ == '__main__':
    unittest.main()

