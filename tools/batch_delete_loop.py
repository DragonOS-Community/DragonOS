import os

start = int(input("Start from: "))
end = int(input("End at: "))

for i in range(start, end+1):
    print("Deleting: " + str(i))
    os.system("sudo losetup -d /dev/loop" + str(i))
print("Done!")