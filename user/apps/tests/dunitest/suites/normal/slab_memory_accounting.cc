#include <gtest/gtest.h>

#include <sys/sysinfo.h>

#include <algorithm>
#include <cstdint>
#include <fstream>
#include <string>
#include <unordered_map>

namespace {

bool ReadMeminfo(std::unordered_map<std::string, uint64_t>* values) {
  std::ifstream input("/proc/meminfo");
  std::string key;
  uint64_t value;
  std::string unit;
  while (input >> key >> value >> unit) {
    if (!key.empty() && key.back() == ':') key.pop_back();
    (*values)[key] = value;
  }
  return !values->empty();
}

bool ReadVmstat(const char* wanted, uint64_t* value) {
  std::ifstream input("/proc/vmstat");
  std::string key;
  uint64_t current;
  while (input >> key >> current) {
    if (key == wanted) {
      *value = current;
      return true;
    }
  }
  return false;
}

uint64_t SysinfoFreeKilobytes(const struct sysinfo& info) {
  const unsigned __int128 bytes =
      static_cast<unsigned __int128>(info.freeram) * info.mem_unit;
  return static_cast<uint64_t>(bytes >> 10);
}

uint64_t AbsoluteDifference(uint64_t left, uint64_t right) {
  return left > right ? left - right : right - left;
}

TEST(SlabMemoryAccounting, MeminfoReportsResidentSlabAsUnreclaimable) {
  std::unordered_map<std::string, uint64_t> values;
  ASSERT_TRUE(ReadMeminfo(&values));
  ASSERT_TRUE(values.count("Slab"));
  ASSERT_TRUE(values.count("SReclaimable"));
  ASSERT_TRUE(values.count("SUnreclaim"));

  EXPECT_GT(values["Slab"], 0U);
  EXPECT_EQ(values["Slab"] % 4U, 0U);
  EXPECT_EQ(values["SReclaimable"], 0U);
  EXPECT_EQ(values["SUnreclaim"], values["Slab"]);
}

TEST(SlabMemoryAccounting, VmstatReportsResidentSlabPages) {
  uint64_t reclaimable = 0;
  uint64_t unreclaimable = 0;
  ASSERT_TRUE(ReadVmstat("nr_slab_reclaimable", &reclaimable));
  ASSERT_TRUE(ReadVmstat("nr_slab_unreclaimable", &unreclaimable));

  EXPECT_EQ(reclaimable, 0U);
  EXPECT_GT(unreclaimable, 0U);
}

TEST(SlabMemoryAccounting, SysinfoReportsSaneBuddyMemory) {
  struct sysinfo before = {};
  struct sysinfo after = {};
  ASSERT_EQ(sysinfo(&before), 0);

  std::unordered_map<std::string, uint64_t> values;
  ASSERT_TRUE(ReadMeminfo(&values));
  ASSERT_TRUE(values.count("MemFree"));
  ASSERT_EQ(sysinfo(&after), 0);

  EXPECT_GT(before.mem_unit, 0U);
  EXPECT_GT(before.totalram, 0U);
  EXPECT_LE(before.freeram, before.totalram);
  EXPECT_EQ(after.mem_unit, before.mem_unit);

  const uint64_t memfree_kb = values["MemFree"];
  const uint64_t delta_before =
      AbsoluteDifference(SysinfoFreeKilobytes(before), memfree_kb);
  const uint64_t delta_after =
      AbsoluteDifference(SysinfoFreeKilobytes(after), memfree_kb);
  // These are independent snapshots. Allow bounded allocator/scheduler churn,
  // while still rejecting the old multi-megabyte slab-free overcount.
  EXPECT_LE(std::min(delta_before, delta_after), 256U);
}

}  // namespace

int main(int argc, char** argv) {
  ::testing::InitGoogleTest(&argc, argv);
  return RUN_ALL_TESTS();
}
