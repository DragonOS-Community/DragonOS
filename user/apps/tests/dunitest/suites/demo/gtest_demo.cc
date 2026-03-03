#include <gtest/gtest.h>

TEST(DemoSuite, BasicArithmetic) {
  int lhs = 1 + 1;
  int rhs = 2;
  EXPECT_EQ(lhs, rhs);
}

TEST(DemoSuite, StringCompare) {
  const char* actual = "dragonos";
  const char* expected = "dragonos";
  EXPECT_STREQ(actual, expected);
}

int main(int argc, char** argv) {
  ::testing::InitGoogleTest(&argc, argv);
  return RUN_ALL_TESTS();
}
