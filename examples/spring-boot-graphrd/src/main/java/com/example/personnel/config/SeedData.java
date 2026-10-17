package com.example.personnel.config;

import java.util.List;

import org.springframework.boot.CommandLineRunner;
import org.springframework.context.annotation.Bean;
import org.springframework.context.annotation.Configuration;

import com.example.personnel.model.entity.Company;
import com.example.personnel.model.entity.Employee;
import com.graphrd.rdf.RdfEntityManager;
import com.example.personnel.repository.CompaniesRepository;
import com.example.personnel.repository.EmployeeRepository;

/** A few companies and employees when the repository is empty. */
@Configuration
class SeedData {

    @Bean
    CommandLineRunner seed(CompaniesRepository companies, EmployeeRepository employees,
                            RdfEntityManager em) {
        return args -> {
            if (employees.count() > 0) {
                return;
            }
            Company acme = new Company(em.namespace() + "company/acme", "Acme", "manufacturing");
            Company initech = new Company(em.namespace() + "company/initech", "Initech", "software");
            Company globex = new Company(em.namespace() + "company/globex", "Globex", "energy");
            companies.save(acme);
            companies.save(initech);
            companies.save(globex);
            employees.save(new Employee(em.namespace() + "employee/alice", "Alice", "alice@example.org", 34, 82000, "2021-03-15", acme));
            employees.save(new Employee(em.namespace() + "employee/bob", "Bob", "bob@example.org", 28, 61000, "2023-07-01", acme));
            employees.save(new Employee(em.namespace() + "employee/carol", "Carol", "carol@example.org", 41, 95000, "2019-11-20", initech));
            employees.save(new Employee(em.namespace() + "employee/dave", "Dave", "dave@example.org", 52, 58000, "2015-05-04", initech));
            employees.save(new Employee(em.namespace() + "employee/erin", "Erin", "erin@example.org", 37, 71000, "2020-09-30", globex));
        };
    }
}
