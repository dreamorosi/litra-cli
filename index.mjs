import { findDevice, turnOff, turnOn, isOn } from "litra";
import { writeFile, mkdir } from "node:fs/promises";
import { homedir } from "node:os";
import { join } from "node:path";

const createConfigFolderIfNotExists = async () => {
  const folder = join(homedir(), ".config", "litra");
  try {
    await mkdir(folder);
  } catch (e) {
    if (e.code !== "EEXIST") {
      throw e;
    }
  }
};

(async () => {
  const device = findDevice();
  await createConfigFolderIfNotExists();
  if (device) {
    if (isOn(device)) {
      turnOff(device);
      await writeFile(join(homedir(), ".config", "litra", "state.txt"), "off");
    } else {
      turnOn(device);
      await writeFile(join(homedir(), ".config", "litra", "state.txt"), "on");
    }
  }
})();
