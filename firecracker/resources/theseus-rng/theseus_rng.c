// SPDX-License-Identifier: GPL-2.0
// Install the Theseus replay seed into Linux's normal CRNG.

#include <linux/module.h>
#include <linux/of.h>
#include <linux/random.h>

#define THESEUS_SEED_LEN 32

static int __init theseus_rng_init(void)
{
	struct device_node *chosen;
	u8 seed[THESEUS_SEED_LEN];
	int ret;

	chosen = of_find_node_by_path("/chosen");
	if (!chosen)
		return -ENODEV;

	ret = of_property_read_u8_array(chosen, "theseus,rng-seed", seed, sizeof(seed));
	of_node_put(chosen);
	if (ret)
		return ret;

	ret = theseus_install_crng_key(seed, sizeof(seed));
	memzero_explicit(seed, sizeof(seed));
	if (!ret)
		pr_info("theseus_rng: installed deterministic CRNG key\n");
	return ret;
}

module_init(theseus_rng_init);

MODULE_DESCRIPTION("Theseus deterministic Linux RNG seed loader");
MODULE_LICENSE("GPL");
