package com.example.personnel.service;

import static org.junit.jupiter.api.Assertions.assertEquals;
import static org.junit.jupiter.api.Assertions.assertThrows;
import static org.junit.jupiter.api.Assertions.assertTrue;

import org.junit.jupiter.api.AfterAll;
import org.junit.jupiter.api.BeforeAll;
import org.junit.jupiter.api.BeforeEach;
import org.junit.jupiter.api.Test;
import org.springframework.test.context.TestConstructor;
import org.springframework.test.context.TestConstructor.AutowireMode;
import org.springframework.beans.factory.annotation.Autowired;
import org.springframework.boot.test.context.SpringBootTest;
import org.springframework.test.context.DynamicPropertyRegistry;
import org.springframework.test.context.DynamicPropertySource;

import com.example.personnel.TestServer;
import com.example.personnel.model.dto.CompanyIn;
import com.example.personnel.model.entity.Company;

/** The company logic against a live GraphRD: minting, dedup by IRI, listing. */
@SpringBootTest(webEnvironment = SpringBootTest.WebEnvironment.NONE)
@TestConstructor(autowireMode = AutowireMode.ALL)
class CompanyServiceIT {

    @BeforeAll
    static void up() throws Exception {
        System.setProperty("graphrd.test.url", TestServer.start());
    }

    @DynamicPropertySource
    static void pointing(DynamicPropertyRegistry registry) {
        registry.add("graphrd.base-url", () -> System.getProperty("graphrd.test.url"));
        registry.add("graphrd.repository", () -> "co-it");
    }

    @AfterAll
    static void down() {
        TestServer.stop();
    }

    private final CompanyService companies;

    CompanyServiceIT(CompanyService companies) {
        this.companies = companies;
    }

    @BeforeEach
    void fresh() {
        companies.repository().deleteAll();
    }

    @Test
    void mintsTheIriFromTheName() {
        Company c = companies.create(new CompanyIn("Initech Corp", "software"));
        assertTrue(c.getId().endsWith("/company/initech-corp"), c.getId());
        assertEquals("Initech Corp", c.getName());
    }

    @Test
    void blankNameIsRejected() {
        assertThrows(IllegalArgumentException.class, () -> companies.create(new CompanyIn(" ", null)));
        assertThrows(IllegalArgumentException.class, () -> companies.create(new CompanyIn(null, null)));
    }

    @Test
    void sameNameLandsOnTheSameIri() {
        companies.create(new CompanyIn("Acme", null));
        Company again = companies.create(new CompanyIn("Acme", "updated"));
        assertEquals(1, companies.all().size());
        assertEquals("updated", again.getIndustry()); // the second save replaced the first
    }
}
