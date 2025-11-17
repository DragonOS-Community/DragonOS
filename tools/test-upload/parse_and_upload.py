#!/usr/bin/env python3
"""
解析 serial_opt.txt 日志文件并上报测试结果到后端API

用法:
    python parse_and_upload.py <log_file> <api_url> --branch <branch_name> --commit <commit_id> [--test-type <test_type>] [--dry-run]

环境变量:
    API_KEY: 后端API的认证密钥（非dry-run模式需要）

示例:
    # 正常上传模式
    export API_KEY=your_api_key_here
    python parse_and_upload.py serial_opt.txt http://localhost:8080/api/v1 --branch main --commit abc123def456
    
    # Dry-run模式（调试用，不需要API_KEY）
    python parse_and_upload.py serial_opt.txt http://localhost:8080/api/v1 --branch main --commit abc123def456 --dry-run
"""

import os
import sys
import re
import argparse
import json
import requests
from typing import List, Dict, Optional
from pathlib import Path


class TestCaseParser:
    """测试用例解析器基类"""
    
    def parse(self, content: str) -> List[Dict]:
        """
        解析日志内容，返回测试用例列表
        
        Returns:
            List[Dict]: 测试用例列表，每个用例包含:
                - name: 测试用例名称
                - status: 状态 (passed/failed/skipped)
                - duration_ms: 执行时长（毫秒）
                - error_log: 错误日志（可选）
                - debug_log: 调试日志（可选）
        """
        raise NotImplementedError


class GoTestParser(TestCaseParser):
    """Go test 日志解析器"""
    
    def parse(self, content: str) -> List[Dict]:
        test_cases = []
        
        # Go test 格式示例:
        # === RUN   TestExample
        # --- PASS: TestExample (0.01s)
        # --- FAIL: TestExample (0.01s)
        # --- SKIP: TestExample (0.01s)
        
        test_pattern = re.compile(
            r'^=== RUN\s+(.+)$',
            re.MULTILINE
        )
        
        result_pattern = re.compile(
            r'^--- (PASS|FAIL|SKIP):\s+(.+?)\s+\(([\d.]+)s\)$',
            re.MULTILINE
        )
        
        # 找到所有测试用例
        test_matches = list(test_pattern.finditer(content))
        result_matches = list(result_pattern.finditer(content))
        
        # 创建结果映射
        result_map = {}
        for match in result_matches:
            status = match.group(1).lower()
            name = match.group(2).strip()
            duration = float(match.group(3))
            duration_ms = int(duration * 1000)
            
            # 状态映射
            status_map = {
                'pass': 'passed',
                'fail': 'failed',
                'skip': 'skipped'
            }
            status = status_map.get(status, 'failed')
            
            result_map[name] = {
                'status': status,
                'duration_ms': duration_ms
            }
        
        # 提取错误日志
        error_sections = re.findall(
            r'--- FAIL:\s+(.+?)\s+\([\d.]+s\)\n(.*?)(?=---|===|\Z)',
            content,
            re.MULTILINE | re.DOTALL
        )
        
        error_map = {}
        for name, error_content in error_sections:
            name = name.strip()
            # 截断到2048字符
            error_log = error_content.strip()[:2048]
            error_map[name] = error_log
        
        # 构建测试用例列表
        processed_names = set()
        for match in test_matches:
            name = match.group(1).strip()
            if name in processed_names:
                continue
            processed_names.add(name)
            
            if name in result_map:
                test_case = result_map[name].copy()
                test_case['name'] = name
                if name in error_map:
                    test_case['error_log'] = error_map[name]
                test_cases.append(test_case)
        
        return test_cases


class PytestParser(TestCaseParser):
    """pytest 日志解析器"""
    
    def parse(self, content: str) -> List[Dict]:
        test_cases = []
        
        # pytest 格式示例:
        # test_example.py::test_function PASSED [ 10%]
        # test_example.py::test_function FAILED [ 10%]
        # test_example.py::test_function SKIPPED [ 10%]
        
        pytest_pattern = re.compile(
            r'^(.+?)::(.+?)\s+(PASSED|FAILED|SKIPPED|ERROR)(?:\s+\[.*?\])?(?:\s+\[([\d.]+)s\])?$',
            re.MULTILINE
        )
        
        matches = pytest_pattern.finditer(content)
        for match in matches:
            file_name = match.group(1)
            test_name = match.group(2)
            status = match.group(3).lower()
            duration = match.group(4)
            
            # 状态映射
            status_map = {
                'passed': 'passed',
                'failed': 'failed',
                'skipped': 'skipped',
                'error': 'failed'
            }
            status = status_map.get(status, 'failed')
            
            # 完整测试用例名称
            full_name = f"{file_name}::{test_name}"
            
            # 解析时长
            duration_ms = 0
            if duration:
                try:
                    duration_ms = int(float(duration) * 1000)
                except ValueError:
                    pass
            
            test_case = {
                'name': full_name,
                'status': status,
                'duration_ms': duration_ms
            }
            
            # 提取错误信息
            if status == 'failed':
                # 查找失败详情
                error_pattern = re.compile(
                    rf'FAILED\s+{re.escape(file_name)}::{re.escape(test_name)}.*?\n(.*?)(?=\n\S|\Z)',
                    re.MULTILINE | re.DOTALL
                )
                error_match = error_pattern.search(content, pos=match.end())
                if error_match:
                    error_log = error_match.group(1).strip()[:2048]
                    test_case['error_log'] = error_log
            
            test_cases.append(test_case)
        
        return test_cases


class GoogleTestParser(TestCaseParser):
    """Google Test (gtest) 日志解析器"""
    
    def parse(self, content: str) -> List[Dict]:
        """
        解析Google Test格式的日志
        
        格式:
        [ RUN      ] TestName
        [       OK ] TestName (4 ms)
        [  FAILED  ] TestName (4 ms)
        
        注意: [ RUN ] 和 [ FAILED ] 可能不在行首（因为并发日志输出）
        """
        test_cases = []
        
        # 匹配 [ RUN      ] TestName（可能不在行首）
        # TestName是连续无空格的字符串
        run_pattern = re.compile(
            r'\[ RUN\s+\]\s+(\S+)',
            re.MULTILINE
        )
        
        # 匹配 [       OK ] TestName (时间 ms)（可能不在行首）
        ok_pattern = re.compile(
            r'\[\s+OK\s+\]\s+(\S+)\s+\((\d+)\s+ms\)',
            re.MULTILINE
        )
        
        # 匹配 [  FAILED  ] TestName (时间 ms)（可能不在行首）
        # 注意：FAILED前面可能有空格，后面也可能有空格
        failed_pattern = re.compile(
            r'\[\s+FAILED\s+\]\s+(\S+)\s+\((\d+)\s+ms\)',
            re.MULTILINE
        )
        
        # 找到所有RUN标记及其位置
        run_matches = list(run_pattern.finditer(content))
        
        # 创建结果映射（OK和FAILED）
        result_map = {}
        for match in ok_pattern.finditer(content):
            name = match.group(1).strip()
            duration_ms = int(match.group(2))
            result_map[match.start()] = {
                'name': name,
                'status': 'passed',
                'duration_ms': duration_ms,
                'end_pos': match.end()
            }
        
        for match in failed_pattern.finditer(content):
            name = match.group(1).strip()
            duration_ms = int(match.group(2))
            result_map[match.start()] = {
                'name': name,
                'status': 'failed',
                'duration_ms': duration_ms,
                'end_pos': match.end()
            }
        
        # 处理每个RUN标记
        for run_match in run_matches:
            test_name = run_match.group(1).strip()
            run_start = run_match.start()
            run_end = run_match.end()
            
            # 查找对应的结果（OK或FAILED）
            # 结果应该在RUN之后
            result = None
            result_start = None
            
            for pos, res in result_map.items():
                if pos > run_start and res['name'] == test_name:
                    result = res
                    result_start = pos
                    break
            
            # 构建测试用例
            test_case = {
                'name': test_name,
                'status': 'failed',  # 默认失败（如果没有找到结果）
                'duration_ms': 0
            }
            
            if result:
                # 找到了结果（OK或FAILED）
                test_case['status'] = result['status']
                test_case['duration_ms'] = result['duration_ms']
                
                # 如果是失败，提取错误日志
                if result['status'] == 'failed':
                    # 从RUN之后到FAILED之前的内容作为错误日志
                    error_start = run_end
                    error_end = result_start
                    error_content = content[error_start:error_end].strip()
                    
                    # 查找test/开头的错误信息（更精确的错误信息）
                    # 匹配从test/开始到下一个[标记或文件结束的所有内容
                    # 使用更宽松的模式，匹配多行错误信息
                    test_error_pattern = re.compile(
                        r'(test/[^\n:]+:\d+:[^\n]*(?:\n(?!\[)[^\n]*)*)',
                        re.MULTILINE
                    )
                    test_error_match = test_error_pattern.search(content, pos=error_start, endpos=error_end)
                    
                    if test_error_match:
                        # 提取匹配的内容，但不要超过error_end
                        match_start = test_error_match.start()
                        # 尝试提取到error_end之前的所有内容
                        potential_end = test_error_match.end()
                        # 如果匹配的内容不够完整，使用error_content
                        if potential_end < error_end - 50:  # 如果还有50字符以上的内容
                            # 使用完整的error_content
                            error_log = error_content
                        else:
                            match_end = min(potential_end, error_end)
                            error_log = content[match_start:match_end].strip()
                    else:
                        error_log = error_content
                    
                    # 截断到2048字符
                    if error_log:
                        test_case['error_log'] = error_log[:2048]
            else:
                # 没有找到结果（失败场景1：只有RUN没有结果）
                # 查找错误信息（test/开头的文件路径）
                # 从RUN之后查找，直到下一个RUN或文件结束
                next_run_start = len(content)
                for next_run in run_matches:
                    if next_run.start() > run_start:
                        next_run_start = next_run.start()
                        break
                
                # 查找test/开头的错误信息
                test_error_pattern = re.compile(
                    r'(test/[^\n:]+:\d+:[^\n]*(?:\n(?!\[)[^\n]*)*)',
                    re.MULTILINE
                )
                test_error_match = test_error_pattern.search(content, pos=run_end, endpos=next_run_start)
                
                if test_error_match:
                    # 提取匹配的内容，但不要超过next_run_start
                    match_start = test_error_match.start()
                    # 尝试提取更多内容，直到下一个[或next_run_start
                    # 从match_start到next_run_start之间的所有内容
                    error_log = content[match_start:next_run_start].strip()
                    if error_log:
                        test_case['error_log'] = error_log[:2048]
                else:
                    # 如果没有找到test/格式的错误，使用RUN到下一个RUN之间的内容
                    error_content = content[run_end:next_run_start].strip()
                    # 过滤掉一些明显不是错误信息的内容
                    lines = error_content.split('\n')
                    error_lines = []
                    for line in lines:
                        line = line.strip()
                        # 跳过空行、调试信息等
                        if line and not line.startswith('[DEBUG]') and not line.startswith('[INFO]'):
                            error_lines.append(line)
                    
                    if error_lines:
                        error_log = '\n'.join(error_lines[:20])  # 最多20行
                        test_case['error_log'] = error_log[:2048]
            
            test_cases.append(test_case)
        
        return test_cases


class GenericParser(TestCaseParser):
    """通用解析器，尝试多种格式"""
    
    def parse(self, content: str) -> List[Dict]:
        # 先尝试 Google Test 格式（最常见）
        gtest_parser = GoogleTestParser()
        test_cases = gtest_parser.parse(content)
        
        if test_cases:
            return test_cases
        
        # 再尝试 Go test 格式
        go_parser = GoTestParser()
        test_cases = go_parser.parse(content)
        
        if test_cases:
            return test_cases
        
        # 再尝试 pytest 格式
        pytest_parser = PytestParser()
        test_cases = pytest_parser.parse(content)
        
        if test_cases:
            return test_cases
        
        # 如果都不匹配，返回空列表
        return []


def parse_log_file(file_path: str) -> List[Dict]:
    """
    解析日志文件
    
    Args:
        file_path: 日志文件路径
        
    Returns:
        测试用例列表
    """
    path = Path(file_path)
    if not path.exists():
        raise FileNotFoundError(f"日志文件不存在: {file_path}")
    
    # 尝试以文本方式读取
    try:
        with open(path, 'r', encoding='utf-8') as f:
            content = f.read()
    except UnicodeDecodeError:
        # 如果UTF-8失败，尝试其他编码
        try:
            with open(path, 'r', encoding='latin-1') as f:
                content = f.read()
        except Exception as e:
            raise ValueError(f"无法读取日志文件: {e}")
    
    if not content.strip():
        print("警告: 日志文件为空")
        return []
    
    # 使用通用解析器
    parser = GenericParser()
    test_cases = parser.parse(content)
    
    return test_cases


def upload_test_results(
    api_url: str,
    api_key: str,
    branch_name: str,
    commit_id: str,
    test_cases: List[Dict],
    test_type: str = "gvisor"
) -> Dict:
    """
    上传测试结果到后端API
    
    Args:
        api_url: API基础URL
        api_key: API密钥
        branch_name: 分支名称
        commit_id: Commit ID
        test_cases: 测试用例列表
        test_type: 测试类型，默认为gvisor
        
    Returns:
        API响应数据
    """
    # 构建完整URL
    if not api_url.endswith('/test-runs'):
        if api_url.endswith('/'):
            url = f"{api_url}test-runs"
        else:
            url = f"{api_url}/test-runs"
    else:
        url = api_url
    
    # 根据测试用例状态确定整体状态
    status = "passed"
    for tc in test_cases:
        if tc.get('status') == 'failed':
            status = "failed"
            break
    
    # 构建请求数据
    payload = {
        "branch_name": branch_name,
        "commit_id": commit_id,
        "test_type": test_type,
        "status": status,
        "test_cases": test_cases
    }
    
    # 设置请求头
    headers = {
        "Authorization": f"Bearer {api_key}",
        "Content-Type": "application/json"
    }
    
    # 发送请求
    try:
        response = requests.post(url, json=payload, headers=headers, timeout=30)
        response.raise_for_status()
        return response.json()
    except requests.exceptions.RequestException as e:
        if hasattr(e, 'response') and e.response is not None:
            try:
                error_data = e.response.json()
                error_msg = error_data.get('message', str(e))
            except (ValueError, json.JSONDecodeError):
                error_msg = e.response.text or str(e)
        else:
            error_msg = str(e)
        raise Exception(f"上传失败: {error_msg}")


def print_test_cases_details(test_cases: List[Dict]):
    """打印测试用例详细信息"""
    print("\n" + "="*80)
    print("测试用例详情:")
    print("="*80)
    
    for i, tc in enumerate(test_cases, 1):
        print(f"\n[{i}/{len(test_cases)}] {tc.get('name', 'N/A')}")
        print(f"  状态: {tc.get('status', 'N/A')}")
        print(f"  耗时: {tc.get('duration_ms', 0)} ms")
        
        if tc.get('error_log'):
            error_log = tc.get('error_log', '')
            # 如果错误日志太长，只显示前500字符
            if len(error_log) > 500:
                print(f"  错误日志: {error_log[:500]}... (共{len(error_log)}字符)")
            else:
                print(f"  错误日志: {error_log}")
        
        if tc.get('debug_log'):
            debug_log = tc.get('debug_log', '')
            # 如果调试日志太长，只显示前500字符
            if len(debug_log) > 500:
                print(f"  调试日志: {debug_log[:500]}... (共{len(debug_log)}字符)")
            else:
                print(f"  调试日志: {debug_log}")
    
    print("\n" + "="*80)


def main():
    parser = argparse.ArgumentParser(
        description='解析测试日志并上报到后端API',
        formatter_class=argparse.RawDescriptionHelpFormatter,
        epilog=__doc__
    )
    
    parser.add_argument(
        'log_file',
        help='日志文件路径 (serial_opt.txt)'
    )
    
    parser.add_argument(
        'api_url',
        help='后端API地址 (例如: http://localhost:8080/api/v1)'
    )
    
    parser.add_argument(
        '--branch',
        '--branch-name',
        dest='branch_name',
        required=True,
        help='Git分支名称 (例如: main, dev)'
    )
    
    parser.add_argument(
        '--commit',
        '--commit-id',
        dest='commit_id',
        required=True,
        help='Commit ID (至少8位)'
    )
    
    parser.add_argument(
        '--test-type',
        dest='test_type',
        default='gvisor',
        help='测试类型 (默认: gvisor)'
    )
    
    parser.add_argument(
        '--dry-run',
        action='store_true',
        help='干运行模式：只解析并显示结果，不上传到服务器'
    )
    
    args = parser.parse_args()
    
    # 在dry-run模式下，不需要API Key
    if not args.dry_run:
        api_key = os.environ.get('API_KEY')
        if not api_key:
            print("错误: 未设置环境变量 API_KEY", file=sys.stderr)
            sys.exit(1)
    else:
        api_key = None
        print("="*80)
        print("DRY-RUN 模式: 只解析日志，不会上传到服务器")
        print("="*80)
    
    # 验证commit_id长度
    if len(args.commit_id) < 8:
        print("错误: commit_id 至少需要8位字符", file=sys.stderr)
        sys.exit(1)
    
    try:
        # 解析日志文件
        print(f"\n正在解析日志文件: {args.log_file}")
        test_cases = parse_log_file(args.log_file)
        
        if not test_cases:
            print("警告: 未找到任何测试用例", file=sys.stderr)
            sys.exit(1)
        
        print(f"✓ 找到 {len(test_cases)} 个测试用例")
        
        # 显示解析结果摘要
        status_count = {}
        for tc in test_cases:
            status = tc.get('status', 'unknown')
            status_count[status] = status_count.get(status, 0) + 1
        
        print("\n测试用例状态统计:")
        for status, count in sorted(status_count.items()):
            print(f"  {status}: {count}")
        
        # 根据测试用例状态确定整体状态
        overall_status = "passed"
        for tc in test_cases:
            if tc.get('status') == 'failed':
                overall_status = "failed"
                break
        
        print(f"\n整体状态: {overall_status}")
        
        # 构建将要上传的payload
        payload = {
            "branch_name": args.branch_name,
            "commit_id": args.commit_id,
            "test_type": args.test_type,
            "status": overall_status,
            "test_cases": test_cases
        }
        
        # 在dry-run模式下，显示详细信息
        if args.dry_run:
            # 显示测试用例详情
            print_test_cases_details(test_cases)
            
            # 显示将要上传的JSON
            print("\n" + "="*80)
            print("将要上传的JSON数据:")
            print("="*80)
            print(json.dumps(payload, indent=2, ensure_ascii=False))
            
            # 显示API信息
            if not args.api_url.endswith('/test-runs'):
                if args.api_url.endswith('/'):
                    url = f"{args.api_url}test-runs"
                else:
                    url = f"{args.api_url}/test-runs"
            else:
                url = args.api_url
            
            print("\n" + "="*80)
            print(f"目标API地址: {url}")
            print(f"请求方法: POST")
            print(f"Content-Type: application/json")
            print("="*80)
            print("\n✓ Dry-run 完成，未实际上传数据")
            return
        
        # 实际上传
        print(f"\n正在上传到: {args.api_url}")
        result = upload_test_results(
            api_url=args.api_url,
            api_key=api_key,
            branch_name=args.branch_name,
            commit_id=args.commit_id,
            test_cases=test_cases,
            test_type=args.test_type
        )
        
        # 显示结果
        if result.get('code') == 200:
            data = result.get('data', {})
            test_run_id = data.get('id')
            print(f"\n✓ 上传成功!")
            print(f"  测试运行ID: {test_run_id}")
            print(f"  分支: {data.get('branch_name')}")
            print(f"  Commit: {data.get('commit_short_id')}")
            print(f"  状态: {data.get('status')}")
        else:
            print(f"\n✗ 上传失败: {result.get('message', '未知错误')}", file=sys.stderr)
            sys.exit(1)
            
    except FileNotFoundError as e:
        print(f"错误: {e}", file=sys.stderr)
        sys.exit(1)
    except Exception as e:
        print(f"错误: {e}", file=sys.stderr)
        sys.exit(1)


if __name__ == '__main__':
    main()

